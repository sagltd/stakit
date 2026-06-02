//! The agent: a builder over a [`Provider`] + [`ToolRegistry`], and the loop
//! that drives them as an async event stream.
//!
//! The loop is the canonical cycle — call the model, stream its output, run any
//! requested tools (gated by `can_use_tool`, with `Ask` routed to `on_ask`),
//! append the results, repeat — until the model ends its turn or a `stop_when`
//! condition matches. It is cancellable mid-stream via a [`CancelToken`].

use std::sync::Arc;

use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::Stream;
use serde_json::{Value, json};

use crate::cache::CacheStrategy;
use crate::cancel::CancelToken;
use crate::context::ContextLoader;
use crate::cx::{Permission, ToolCx};
use crate::loop_event::{FinishReason, LoopEvent, StopCond};
use crate::message::{
    AssistantContent, Message, SystemPrompt, Thinking, ToolResultPart, UserContent,
};
use crate::provider::{
    ChatRequest, Provider, StopReason, StreamEvent, ThinkingConfig, ToolChoice, ToolDef,
};

use crate::skill::{SkillLoader, SkillManifest};

/// Name of the built-in tool-search tool (offered when deferred tools exist).
const TOOL_SEARCH: &str = "tool_search";
/// Name of the built-in skill-load tool (offered when a skill loader is set).
const LOAD_SKILL: &str = "load_skill";
/// Name of the built-in skill-search tool.
const SEARCH_SKILLS: &str = "search_skills";
use crate::tool::{Tool, ToolRegistry, ToolSet, TypedTool};
use crate::usage::{Pricing, Usage};

/// An async permission hook: `(tool_name, args, cx) -> Permission`.
type Hook<Ctx> = Arc<
    dyn for<'a> Fn(&'a str, &'a Value, &'a ToolCx<Ctx>) -> BoxFuture<'a, Permission> + Send + Sync,
>;

/// A synchronous hook run before each step to rewrite history (compaction,
/// injection, pruning). Receives the step number and mutable history.
type PrepareStep = Arc<dyn Fn(u32, &mut Vec<Message>) + Send + Sync>;

struct Inner<P, Ctx> {
    provider: P,
    /// Optional model override; defaults to the provider's `model_id()`.
    model: Option<String>,
    system: Option<crate::message::SystemPrompt>,
    tools: ToolRegistry<Ctx>,
    pricing: Pricing,
    max_tokens: u32,
    temperature: Option<f32>,
    tool_choice: ToolChoice,
    cache: CacheStrategy,
    thinking: ThinkingConfig,
    stop_when: Vec<StopCond>,
    can_use_tool: Option<Hook<Ctx>>,
    on_ask: Option<Hook<Ctx>>,
    prepare_step: Option<PrepareStep>,
    context_loaders: Vec<Arc<dyn ContextLoader<Ctx>>>,
    skills: Option<Arc<dyn SkillLoader<Ctx>>>,
}

/// A composed agent. Cheap to clone (shared inner state).
#[derive(Clone)]
pub struct Agent<P, Ctx> {
    inner: Arc<Inner<P, Ctx>>,
}

impl<P: Provider, Ctx: Send + Sync + 'static> Agent<P, Ctx> {
    /// Starts building an agent over `provider`.
    pub fn builder(provider: P) -> AgentBuilder<P, Ctx> {
        AgentBuilder::new(provider)
    }

    /// Registers a tool on a live agent (the registry is shared across clones).
    /// Takes effect on the next step — call before/between runs.
    pub fn register_tool<T: Tool<Ctx>>(&self, tool: T) {
        self.inner.tools.register(tool);
    }

    /// Registers a bundle of tools on a live agent.
    pub fn register_tool_set<S: ToolSet<Ctx>>(&self, set: S) {
        self.inner.tools.register_set(set);
    }

    /// Removes a tool by name; returns whether it was present.
    pub fn remove_tool(&self, name: &str) -> bool {
        self.inner.tools.remove(name)
    }

    /// Names of the currently registered tools.
    #[must_use]
    pub fn tool_names(&self) -> Vec<String> {
        self.inner.tools.names()
    }

    /// Runs the loop, yielding a stream of [`LoopEvent`]s.
    ///
    /// `history` is the initial conversation, `ctx` the tool context, and
    /// `cancel` aborts the run mid-stream. Cancellation interrupts the provider
    /// stream immediately; a tool already executing runs to completion unless it
    /// observes [`ToolCx::cancel_token`](crate::ToolCx::cancel_token) itself, so
    /// long-running tools should poll it.
    pub fn run(
        &self,
        history: Vec<Message>,
        ctx: Ctx,
        cancel: CancelToken,
    ) -> impl Stream<Item = LoopEvent> + Send {
        self.run_with_input(history, ctx, cancel, None)
    }

    /// Like [`Agent::run`], but drains `input` before each step so the host can
    /// inject user messages mid-loop (pass `None` for no injection).
    #[expect(
        clippy::too_many_lines,
        clippy::similar_names,
        reason = "the loop is one cohesive state machine; splitting it would obscure control flow"
    )]
    pub fn run_with_input(
        &self,
        history: Vec<Message>,
        ctx: Ctx,
        cancel: CancelToken,
        input: Option<tokio::sync::mpsc::UnboundedReceiver<Message>>,
    ) -> impl Stream<Item = LoopEvent> + Send {
        let inner = Arc::clone(&self.inner);
        async_stream::stream! {
            let cx = ToolCx::with_cancel(ctx, cancel.clone());
            let mut history = history;
            let mut input = input;
            let mut total = Usage::default();
            let mut total_cost: Option<f64> = None;
            let mut final_text = String::new();
            let mut step: u32 = 0;
            let finish;

            // Model id: explicit override, else the provider's bound model.
            let model_id = inner
                .model
                .clone()
                .unwrap_or_else(|| inner.provider.model_id().to_owned());

            // Run context loaders once, merging their output into the system
            // prompt and seeding the history.
            let mut system = inner.system.clone();
            {
                let mut seed: Vec<Message> = Vec::new();
                for loader in &inner.context_loaders {
                    match loader.load(&cx).await {
                        Ok(loaded) => {
                            if let Some(text) = loaded.system {
                                match &mut system {
                                    Some(sp) => {
                                        sp.text.push_str("\n\n");
                                        sp.text.push_str(&text);
                                    }
                                    None => system = Some(SystemPrompt::from(text)),
                                }
                            }
                            seed.extend(loaded.messages);
                        }
                        Err(e) => {
                            yield LoopEvent::Done {
                                text: format!("context loader error: {e}"),
                                usage: total,
                                cost: total_cost,
                                reason: FinishReason::Error,
                            };
                            return;
                        }
                    }
                }
                if !seed.is_empty() {
                    seed.append(&mut history);
                    history = seed;
                }
            }

            // List skills once; inject their manifests (name + description) into
            // the system prompt. Bodies load on demand via `load_skill`.
            let mut skill_manifests: Vec<SkillManifest> = Vec::new();
            if let Some(loader) = &inner.skills {
                match loader.list(&cx).await {
                    Ok(manifests) => skill_manifests = manifests,
                    Err(e) => {
                        yield LoopEvent::Done {
                            text: format!("skill loader error: {e}"),
                            usage: total,
                            cost: total_cost,
                            reason: FinishReason::Error,
                        };
                        return;
                    }
                }
                if !skill_manifests.is_empty() {
                    let mut summary = String::from(
                        "## Available skills\nUse `load_skill(name)` to load a skill's full \
                         instructions, or `search_skills(query)` to find one.\n",
                    );
                    for m in &skill_manifests {
                        use std::fmt::Write as _;
                        let _ = writeln!(summary, "- {}: {}", m.name, m.description);
                    }
                    match &mut system {
                        Some(sp) => {
                            sp.text.push_str("\n\n");
                            sp.text.push_str(&summary);
                        }
                        None => system = Some(SystemPrompt::from(summary)),
                    }
                }
            }

            loop {
                if cancel.is_cancelled() {
                    finish = FinishReason::Cancelled;
                    break;
                }
                step += 1;

                // Drain any injected messages, then let prepare_step rewrite history.
                if let Some(rx) = input.as_mut() {
                    while let Ok(msg) = rx.try_recv() {
                        history.push(msg);
                    }
                }
                if let Some(hook) = &inner.prepare_step {
                    hook(step, &mut history);
                }

                yield LoopEvent::StepStart { step };

                let request =
                    build_request(&inner, &model_id, !skill_manifests.is_empty(), system.as_ref(), &history);
                let mut stream = match inner.provider.stream(request).await {
                    Ok(s) => s,
                    Err(e) => {
                        final_text = format!("provider error: {e}");
                        finish = FinishReason::Error;
                        break;
                    }
                };

                let mut text = String::new();
                let mut reasoning = String::new();
                let mut signature: Option<String> = None;
                let mut tool_calls: Vec<(String, String, Value)> = Vec::new();
                let mut step_usage = Usage::default();
                let mut stop = StopReason::EndTurn;
                let mut cancelled = false;
                let mut errored = false;

                loop {
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => { cancelled = true; break; }
                        next = stream.next() => {
                            match next {
                                None => break,
                                Some(Err(e)) => {
                                    final_text = format!("stream error: {e}");
                                    errored = true;
                                    break;
                                }
                                Some(Ok(event)) => match event {
                                    StreamEvent::Start { usage } => step_usage = usage,
                                    StreamEvent::TextDelta(t) => {
                                        text.push_str(&t);
                                        yield LoopEvent::TextDelta(t);
                                    }
                                    StreamEvent::ReasoningDelta(t) => {
                                        reasoning.push_str(&t);
                                        yield LoopEvent::ReasoningDelta(t);
                                    }
                                    StreamEvent::SignatureDelta(s) => {
                                        signature.get_or_insert_with(String::new).push_str(&s);
                                    }
                                    StreamEvent::ToolCall { id, name, input } => {
                                        tool_calls.push((id.clone(), name.clone(), input.clone()));
                                        yield LoopEvent::ToolCall { id, name, input };
                                    }
                                    StreamEvent::End { stop: s, usage } => {
                                        stop = s;
                                        step_usage = usage;
                                    }
                                }
                            }
                        }
                    }
                }

                if cancelled {
                    finish = FinishReason::Cancelled;
                    break;
                }
                if errored {
                    finish = FinishReason::Error;
                    break;
                }

                // Assemble the assistant turn (thinking first, then text, then tool calls).
                let mut blocks: Vec<AssistantContent> = Vec::new();
                if !reasoning.is_empty() {
                    blocks.push(AssistantContent::Thinking(Thinking::Visible {
                        text: reasoning,
                        signature,
                    }));
                }
                if !text.is_empty() {
                    final_text = text.clone();
                    blocks.push(AssistantContent::Text(text));
                }
                for (id, name, input) in &tool_calls {
                    blocks.push(AssistantContent::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    });
                }
                history.push(Message::Assistant(blocks));

                total.merge(&step_usage);
                let step_cost = inner.pricing.cost(&model_id, &step_usage);
                if let Some(c) = step_cost {
                    total_cost = Some(total_cost.unwrap_or(0.0) + c);
                }
                yield LoopEvent::Usage { step, usage: step_usage, cost: step_cost };
                yield LoopEvent::StepEnd { step, stop };

                if tool_calls.is_empty() {
                    finish = FinishReason::EndTurn;
                    break;
                }

                // Run all tool calls in this turn CONCURRENTLY (the model may
                // request several at once), then gather them into one user turn
                // in their original order.
                let called_names: Vec<String> =
                    tool_calls.iter().map(|(_, n, _)| n.clone()).collect();
                let executed = futures::future::join_all(tool_calls.into_iter().map(
                    |(id, name, input)| {
                        let inner = &inner;
                        let cx = &cx;
                        let manifests = &skill_manifests;
                        async move {
                            let (output, is_error) = if name == TOOL_SEARCH {
                                (run_tool_search(inner, &input), false)
                            } else if name == LOAD_SKILL {
                                match run_load_skill(inner, &input, cx).await {
                                    Ok(v) => (v, false),
                                    Err(e) => (json!(e.to_string()), true),
                                }
                            } else if name == SEARCH_SKILLS {
                                (run_search_skills(manifests, &input), false)
                            } else {
                                match decide(inner, &name, &input, cx).await {
                                    Permission::Deny { reason } => (json!(reason), true),
                                    _ => match inner.tools.call(&name, cx, input).await {
                                        Ok(v) => (v, false),
                                        Err(e) => (json!(e.message()), true),
                                    },
                                }
                            };
                            (id, output, is_error)
                        }
                    },
                ))
                .await;

                let mut results: Vec<UserContent> = Vec::with_capacity(executed.len());
                for (id, output, is_error) in executed {
                    yield LoopEvent::ToolResult {
                        id: id.clone(),
                        output: output.clone(),
                        is_error,
                    };
                    results.push(UserContent::ToolResult {
                        id,
                        content: vec![ToolResultPart::Text(value_to_text(&output))],
                        is_error,
                    });
                }
                history.push(Message::User(results));

                if let Some(reason) =
                    stop_now(&inner.stop_when, step, &called_names, total_cost)
                {
                    finish = reason;
                    break;
                }
            }

            yield LoopEvent::Done {
                text: final_text,
                usage: total,
                cost: total_cost,
                reason: finish,
            };
        }
    }
}

fn build_request<P, Ctx: Send + Sync + 'static>(
    inner: &Inner<P, Ctx>,
    model: &str,
    skills_active: bool,
    system: Option<&SystemPrompt>,
    history: &[Message],
) -> ChatRequest {
    let mut tools = inner.tools.active_defs();
    if inner.tools.has_deferred() {
        tools.push(tool_search_def());
    }
    if skills_active {
        tools.extend(skill_tool_defs());
    }
    ChatRequest {
        model: model.to_owned(),
        system: system.cloned(),
        messages: history.to_vec(),
        tools,
        tool_choice: inner.tool_choice.clone(),
        max_tokens: inner.max_tokens,
        temperature: inner.temperature,
        stop_sequences: Vec::new(),
        thinking: inner.thinking,
        cache: inner.cache.clone(),
        extra: serde_json::Map::new(),
    }
}

async fn decide<P, Ctx>(
    inner: &Inner<P, Ctx>,
    name: &str,
    input: &Value,
    cx: &ToolCx<Ctx>,
) -> Permission {
    let base = match &inner.can_use_tool {
        Some(hook) => hook(name, input, cx).await,
        None => Permission::Allow,
    };
    if base == Permission::Ask {
        return match &inner.on_ask {
            Some(hook) => hook(name, input, cx).await,
            None => Permission::Deny {
                reason: "tool requires approval but no resolver is configured".into(),
            },
        };
    }
    base
}

fn stop_now(
    conds: &[StopCond],
    step: u32,
    called: &[String],
    total_cost: Option<f64>,
) -> Option<FinishReason> {
    for cond in conds {
        match cond {
            StopCond::StepCountIs(n) if step >= *n => return Some(FinishReason::MaxSteps),
            StopCond::HasToolCall(name) if called.iter().any(|c| c == name) => {
                return Some(FinishReason::StopCondition);
            }
            StopCond::BudgetUsd(limit) if total_cost.is_some_and(|c| c >= *limit) => {
                return Some(FinishReason::MaxBudget);
            }
            _ => {}
        }
    }
    None
}

/// The built-in tool-search tool definition.
fn tool_search_def() -> ToolDef {
    ToolDef::new(
        TOOL_SEARCH,
        "Search for additional tools by keyword. Matched tools become available to call on the next step.",
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Keywords to match against tool names and descriptions" }
            },
            "required": ["query"]
        }),
    )
}

/// Runs the built-in tool search: finds deferred tools matching the query,
/// activates them, and returns the matches as the tool result.
fn run_tool_search<P, Ctx: Send + Sync + 'static>(inner: &Inner<P, Ctx>, input: &Value) -> Value {
    let query = input.get("query").and_then(Value::as_str).unwrap_or("");
    let matches = inner.tools.search(query);
    for (name, _) in &matches {
        inner.tools.activate(name);
    }
    json!({
        "matches": matches
            .into_iter()
            .map(|(name, description)| json!({ "name": name, "description": description }))
            .collect::<Vec<_>>()
    })
}

/// Built-in skill tool definitions, offered when a skill loader is set.
fn skill_tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef::new(
            LOAD_SKILL,
            "Load a skill's full instructions by name (from the available-skills list).",
            json!({
                "type": "object",
                "properties": { "name": { "type": "string", "description": "Skill name" } },
                "required": ["name"]
            }),
        ),
        ToolDef::new(
            SEARCH_SKILLS,
            "Search available skills by keyword; returns matching names and descriptions.",
            json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"]
            }),
        ),
    ]
}

/// Runs the built-in `load_skill`: fetches one skill's body on demand.
async fn run_load_skill<P, Ctx: Send + Sync + 'static>(
    inner: &Inner<P, Ctx>,
    input: &Value,
    cx: &ToolCx<Ctx>,
) -> Result<Value, crate::error::AiError> {
    let loader = inner
        .skills
        .as_ref()
        .ok_or_else(|| crate::error::AiError::Skill("no skill loader configured".into()))?;
    let name = input
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let content = loader.load(name, cx).await?;
    Ok(json!({ "body": content.body, "references": content.references }))
}

/// Runs the built-in `search_skills` over the listed manifests.
fn run_search_skills(manifests: &[SkillManifest], input: &Value) -> Value {
    let q = input
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    let matches: Vec<Value> = manifests
        .iter()
        .filter(|m| {
            format!("{} {}", m.name, m.description)
                .to_lowercase()
                .contains(&q)
        })
        .map(|m| json!({ "name": m.name, "description": m.description }))
        .collect();
    json!({ "matches": matches })
}

fn value_to_text(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), ToOwned::to_owned)
}

/// Builds an [`Agent`].
pub struct AgentBuilder<P, Ctx> {
    inner: Inner<P, Ctx>,
}

impl<P: Provider, Ctx: Send + Sync + 'static> AgentBuilder<P, Ctx> {
    fn new(provider: P) -> Self {
        Self {
            inner: Inner {
                provider,
                model: None,
                system: None,
                tools: ToolRegistry::new(),
                pricing: Pricing::new(),
                max_tokens: 4096,
                temperature: None,
                tool_choice: ToolChoice::Auto,
                cache: CacheStrategy::Auto,
                thinking: ThinkingConfig::Off,
                stop_when: vec![StopCond::StepCountIs(20)],
                can_use_tool: None,
                on_ask: None,
                prepare_step: None,
                context_loaders: Vec::new(),
                skills: None,
            },
        }
    }

    /// Sets the skill loader. Skill manifests (name + description) are injected
    /// into the system prompt; bodies load on demand via the built-in
    /// `load_skill` / `search_skills` tools (progressive disclosure).
    #[must_use]
    pub fn skills<L: SkillLoader<Ctx> + 'static>(mut self, loader: L) -> Self {
        self.inner.skills = Some(Arc::new(loader));
        self
    }

    /// Adds a context loader. Call repeatedly to load from multiple sources
    /// (file, DB, HTTP, RAG, …); all run before the loop and their output is
    /// merged into the system prompt and seed history.
    #[must_use]
    pub fn context_loader<L: ContextLoader<Ctx> + 'static>(mut self, loader: L) -> Self {
        self.inner.context_loaders.push(Arc::new(loader));
        self
    }

    /// Overrides the model id (defaults to the provider's `model_id()` — you
    /// usually don't need this, since the provider handle already names a model).
    #[must_use]
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.inner.model = Some(model.into());
        self
    }

    /// Sets the system prompt.
    #[must_use]
    pub fn system(mut self, system: impl Into<crate::message::SystemPrompt>) -> Self {
        self.inner.system = Some(system.into());
        self
    }

    /// Registers a typed tool.
    #[must_use]
    pub fn register<T: Tool<Ctx>>(self, tool: T) -> Self {
        self.inner.tools.register(tool);
        self
    }

    /// Registers a bundle of tools.
    #[must_use]
    pub fn register_set<S: ToolSet<Ctx>>(self, set: S) -> Self {
        self.inner.tools.register_set(set);
        self
    }

    /// Registers a deferred tool: withheld from the prompt until the built-in
    /// `tool_search` surfaces it (keeps large tool sets out of the context).
    #[must_use]
    pub fn register_deferred<T: Tool<Ctx>>(self, tool: T) -> Self {
        self.inner
            .tools
            .insert(Arc::new(TypedTool(tool)), Vec::new(), true);
        self
    }

    /// Sets the pricing table (for per-step cost estimates).
    #[must_use]
    pub fn pricing(mut self, pricing: Pricing) -> Self {
        self.inner.pricing = pricing;
        self
    }

    /// Sets the max tokens per step.
    #[must_use]
    pub const fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.inner.max_tokens = max_tokens;
        self
    }

    /// Sets the sampling temperature.
    #[must_use]
    pub const fn temperature(mut self, temperature: f32) -> Self {
        self.inner.temperature = Some(temperature);
        self
    }

    /// Sets the tool-choice policy.
    #[must_use]
    pub fn tool_choice(mut self, tool_choice: ToolChoice) -> Self {
        self.inner.tool_choice = tool_choice;
        self
    }

    /// Sets the cache strategy.
    #[must_use]
    pub fn cache(mut self, cache: CacheStrategy) -> Self {
        self.inner.cache = cache;
        self
    }

    /// Sets the extended-thinking configuration.
    #[must_use]
    pub const fn thinking(mut self, thinking: ThinkingConfig) -> Self {
        self.inner.thinking = thinking;
        self
    }

    /// Replaces the stop conditions (OR-ed). Defaults to `[StepCountIs(20)]`.
    #[must_use]
    pub fn stop_when(mut self, conds: Vec<StopCond>) -> Self {
        self.inner.stop_when = conds;
        self
    }

    /// Sets the permission guard run before each tool call.
    #[must_use]
    pub fn can_use_tool<F>(mut self, hook: F) -> Self
    where
        F: for<'a> Fn(&'a str, &'a Value, &'a ToolCx<Ctx>) -> BoxFuture<'a, Permission>
            + Send
            + Sync
            + 'static,
    {
        self.inner.can_use_tool = Some(Arc::new(hook));
        self
    }

    /// Sets a hook run before each step to rewrite history — inject context,
    /// compact old turns, or prune. Receives the step number and `&mut history`.
    #[must_use]
    pub fn prepare_step<F>(mut self, hook: F) -> Self
    where
        F: Fn(u32, &mut Vec<Message>) + Send + Sync + 'static,
    {
        self.inner.prepare_step = Some(Arc::new(hook));
        self
    }

    /// Sets the resolver for `Permission::Ask` (e.g. human approval).
    #[must_use]
    pub fn on_ask<F>(mut self, hook: F) -> Self
    where
        F: for<'a> Fn(&'a str, &'a Value, &'a ToolCx<Ctx>) -> BoxFuture<'a, Permission>
            + Send
            + Sync
            + 'static,
    {
        self.inner.on_ask = Some(Arc::new(hook));
        self
    }

    /// Finishes building the agent.
    #[must_use]
    pub fn build(self) -> Agent<P, Ctx> {
        Agent {
            inner: Arc::new(self.inner),
        }
    }
}
