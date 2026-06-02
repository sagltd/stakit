//! The typed tool system: [`Tool`], its object-safe twin [`ToolDyn`], the
//! [`ToolRegistry`], and [`ToolSet`] bundles.
//!
//! A [`Tool`] is strongly typed (arguments derive `Model` + `JsonSchema`,
//! output is `Serialize`); wrapping it in [`TypedTool`] erases it to a
//! JSON-in / JSON-out [`ToolDyn`] so heterogeneous tools live in one registry. The
//! registry is flat with optional tags and deferred entries (deferred tools
//! are withheld from the prompt until surfaced by tool search). A "router
//! tool" is simply a `ToolDyn` that owns its own sub-registry — recursion, no
//! new concept.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::future::BoxFuture;
use indexmap::IndexMap;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use stakit_model::{JsonSchema, Validate};

use crate::cx::ToolCx;
use crate::error::ToolError;
use crate::provider::ToolDef;

/// A strongly-typed, context-aware tool.
///
/// Usually produced by `#[tool]`, but hand-implementable. `Args` derives
/// `Model` + `JsonSchema` so its schema and validation come from one source.
pub trait Tool<Ctx>: Send + Sync + 'static {
    /// Argument type (deserialized from the model's JSON, validated, schema-exported).
    type Args: DeserializeOwned + JsonSchema + Validate + Send;
    /// Output type (serialized back into the tool result).
    type Output: Serialize + Send;

    /// The tool's unique name.
    fn name(&self) -> &'static str;
    /// A natural-language description for the model.
    fn description(&self) -> &'static str;
    /// Runs the tool.
    fn run<'a>(
        &'a self,
        cx: &'a ToolCx<Ctx>,
        args: Self::Args,
    ) -> BoxFuture<'a, Result<Self::Output, ToolError>>;
}

/// The object-safe, JSON-in/JSON-out erasure of [`Tool`], stored in the registry.
pub trait ToolDyn<Ctx>: Send + Sync {
    /// The provider-facing definition (name, description, JSON Schema).
    fn def(&self) -> ToolDef;
    /// Deserializes + validates the arguments, runs, and serializes the output.
    fn call_json<'a>(
        &'a self,
        cx: &'a ToolCx<Ctx>,
        args: Value,
    ) -> BoxFuture<'a, Result<Value, ToolError>>;
}

/// Wraps a typed [`Tool`] as an erased [`ToolDyn`].
///
/// A free function (a blanket `impl ToolDyn for T: Tool`) would forbid any other
/// concrete `ToolDyn` impl (e.g. MCP tools) by coherence, so we wrap explicitly.
pub struct TypedTool<T>(pub T);

impl<Ctx, T> ToolDyn<Ctx> for TypedTool<T>
where
    T: Tool<Ctx>,
    Ctx: Send + Sync,
{
    fn def(&self) -> ToolDef {
        ToolDef::new(
            self.0.name(),
            self.0.description(),
            <T::Args as JsonSchema>::schema(),
        )
    }

    fn call_json<'a>(
        &'a self,
        cx: &'a ToolCx<Ctx>,
        args: Value,
    ) -> BoxFuture<'a, Result<Value, ToolError>> {
        Box::pin(async move {
            let parsed: T::Args = serde_json::from_value(args)
                .map_err(|e| ToolError::new(format!("invalid arguments: {e}")))?;
            parsed
                .validate()
                .map_err(|e| ToolError::new(e.to_string()))?;
            let output = self.0.run(cx, parsed).await?;
            serde_json::to_value(output).map_err(|e| ToolError::new(e.to_string()))
        })
    }
}

/// A bundle of tools registered together (an MCP server, a "web tools" group, …).
pub trait ToolSet<Ctx> {
    /// Consumes the set, yielding its erased tools.
    fn into_tools(self) -> Vec<Arc<dyn ToolDyn<Ctx>>>;
}

impl<Ctx> ToolSet<Ctx> for Vec<Arc<dyn ToolDyn<Ctx>>> {
    fn into_tools(self) -> Self {
        self
    }
}

struct Entry<Ctx> {
    tool: Arc<dyn ToolDyn<Ctx>>,
    tags: Vec<String>,
    /// Deferred tools are withheld from the prompt until tool search surfaces
    /// them. Interior-mutable so `activate` works through a shared registry.
    defer: AtomicBool,
}

/// A flat, name-keyed registry of tools with optional tags and deferral.
///
/// Internally synchronized (a `RwLock`), so tools can be added or removed
/// through a shared `&self` — including on a live, cloned [`Agent`] between (or
/// before) runs. Each `run()` reads the current active set at step start.
pub struct ToolRegistry<Ctx> {
    entries: std::sync::RwLock<IndexMap<String, Entry<Ctx>>>,
}

impl<Ctx> Default for ToolRegistry<Ctx> {
    fn default() -> Self {
        Self {
            entries: std::sync::RwLock::new(IndexMap::new()),
        }
    }
}

impl<Ctx: Send + Sync + 'static> ToolRegistry<Ctx> {
    /// A new, empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    // Lock accessors that recover from poisoning (a panicking holder never
    // corrupts the map), so no public method can panic on the lock.
    fn read_lock(&self) -> std::sync::RwLockReadGuard<'_, IndexMap<String, Entry<Ctx>>> {
        self.entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write_lock(&self) -> std::sync::RwLockWriteGuard<'_, IndexMap<String, Entry<Ctx>>> {
        self.entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Registers a typed tool (active, untagged). Overwrites a same-named tool.
    pub fn register<T: Tool<Ctx>>(&self, tool: T) -> &Self {
        self.insert(Arc::new(TypedTool(tool)), Vec::new(), false)
    }

    /// Registers an already-erased tool with explicit tags and deferral.
    pub fn insert(&self, tool: Arc<dyn ToolDyn<Ctx>>, tags: Vec<String>, defer: bool) -> &Self {
        let name = tool.def().name;
        self.write_lock().insert(
            name,
            Entry {
                tool,
                tags,
                defer: AtomicBool::new(defer),
            },
        );
        self
    }

    /// Registers a bundle of tools.
    pub fn register_set<S: ToolSet<Ctx>>(&self, set: S) -> &Self {
        for tool in set.into_tools() {
            self.insert(tool, Vec::new(), false);
        }
        self
    }

    /// Removes a tool by name; returns whether it was present.
    pub fn remove(&self, name: &str) -> bool {
        self.write_lock().shift_remove(name).is_some()
    }

    /// Looks up a tool by name, cloning its handle.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn ToolDyn<Ctx>>> {
        self.read_lock().get(name).map(|e| Arc::clone(&e.tool))
    }

    /// Names of all registered tools, in registration order.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.read_lock().keys().cloned().collect()
    }

    /// Number of registered tools.
    #[must_use]
    pub fn len(&self) -> usize {
        self.read_lock().len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.read_lock().is_empty()
    }

    /// Definitions of the active (non-deferred) tools, in registration order.
    #[must_use]
    pub fn active_defs(&self) -> Vec<ToolDef> {
        self.read_lock()
            .values()
            .filter(|e| !e.defer.load(Ordering::Relaxed))
            .map(|e| e.tool.def())
            .collect()
    }

    /// Whether any tool is currently deferred (so the loop offers tool search).
    #[must_use]
    pub fn has_deferred(&self) -> bool {
        self.read_lock()
            .values()
            .any(|e| e.defer.load(Ordering::Relaxed))
    }

    /// Marks a deferred tool active (called when tool search surfaces it).
    pub fn activate(&self, name: &str) -> bool {
        self.read_lock().get(name).is_some_and(|e| {
            e.defer.store(false, Ordering::Relaxed);
            true
        })
    }

    /// Searches deferred tools by case-insensitive substring over name, tags and
    /// description; returns the `(name, description)` of matches. Mirrors
    /// Anthropic's Tool Search Tool, provider-agnostically.
    #[must_use]
    pub fn search(&self, query: &str) -> Vec<(String, String)> {
        let q = query.to_lowercase();
        self.read_lock()
            .iter()
            .filter(|(_, e)| e.defer.load(Ordering::Relaxed))
            .filter_map(|(name, e)| {
                let def = e.tool.def();
                let hay = format!("{name} {} {}", def.description, e.tags.join(" ")).to_lowercase();
                hay.contains(&q).then_some((name.clone(), def.description))
            })
            .collect()
    }

    /// Runs a tool by name. Returns `Err` (an `is_error` result) for an unknown
    /// tool so the model can recover. The registry lock is released before the
    /// tool body runs (never held across `.await`).
    ///
    /// # Errors
    /// Propagates argument-decode, validation, and tool-body errors.
    pub async fn call(
        &self,
        name: &str,
        cx: &ToolCx<Ctx>,
        args: Value,
    ) -> Result<Value, ToolError> {
        match self.get(name) {
            Some(tool) => tool.call_json(cx, args).await,
            None => Err(ToolError::new(format!("unknown tool: {name}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stakit_model::{JsonSchema, Model};

    #[derive(serde::Deserialize, Model, JsonSchema)]
    struct EchoArgs {
        /// Text to echo back
        #[validate(min_len = 1)]
        text: String,
    }

    struct Echo;

    impl Tool<()> for Echo {
        type Args = EchoArgs;
        type Output = String;

        fn name(&self) -> &'static str {
            "echo"
        }
        fn description(&self) -> &'static str {
            "Echo the text"
        }
        fn run<'a>(
            &'a self,
            _cx: &'a ToolCx<()>,
            args: Self::Args,
        ) -> BoxFuture<'a, Result<Self::Output, ToolError>> {
            Box::pin(async move { Ok(args.text) })
        }
    }

    #[test]
    fn def_carries_schema_from_args() {
        let def = TypedTool(Echo).def();
        assert_eq!(def.name, "echo");
        assert_eq!(def.parameters["type"], "object");
        assert_eq!(def.parameters["properties"]["text"]["type"], "string");
        assert_eq!(
            def.parameters["properties"]["text"]["description"],
            "Text to echo back"
        );
    }

    #[tokio::test]
    async fn registry_calls_tool_by_name() {
        let reg = ToolRegistry::<()>::new();
        reg.register(Echo);
        let cx = ToolCx::new(());
        let out = reg
            .call("echo", &cx, serde_json::json!({ "text": "hi" }))
            .await
            .expect("call ok");
        assert_eq!(out, serde_json::json!("hi"));
    }

    #[tokio::test]
    async fn validation_failure_becomes_tool_error() {
        let cx = ToolCx::new(());
        let err = TypedTool(Echo)
            .call_json(&cx, serde_json::json!({ "text": "" }))
            .await
            .unwrap_err();
        assert!(err.message().contains("text"), "{err}");
    }

    #[test]
    fn deferred_tool_is_hidden_until_search_activates_it() {
        let reg = ToolRegistry::<()>::new();
        reg.insert(Arc::new(TypedTool(Echo)), vec!["text".into()], true);
        assert!(reg.active_defs().is_empty(), "deferred tool must be hidden");
        assert!(reg.has_deferred());

        let hits = reg.search("echo");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "echo");

        assert!(reg.activate("echo"));
        assert_eq!(reg.active_defs().len(), 1);
        assert!(!reg.has_deferred());
    }

    #[tokio::test]
    async fn unknown_tool_is_error() {
        let reg = ToolRegistry::<()>::new();
        let cx = ToolCx::new(());
        let err = reg.call("nope", &cx, Value::Null).await.unwrap_err();
        assert!(err.message().contains("unknown tool"), "{err}");
    }
}
