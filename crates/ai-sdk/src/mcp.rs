//! MCP (Model Context Protocol) client-mode integration.
//!
//! Mirrors how the `OpenAI` Agents SDK and Claude Agent SDK handle MCP: we host
//! the MCP client locally, list its tools, and wrap each as a normal
//! [`ToolDyn`] (namespaced `mcp__<server>__<tool>`) so the agent — and any
//! provider — treats them like native tools, with `can_use_tool` gating intact.
//! Works for stdio and HTTP servers, including private/local ones.
//!
//! The transport is abstracted by [`McpTransport`] so any MCP client (e.g. the
//! `rmcp` crate, or a custom one) plugs in; [`McpToolSet`] adapts a connected
//! transport's tools into the registry.

use std::sync::Arc;

use futures::future::BoxFuture;
use hashbrown::HashMap;
use serde::Deserialize;
use serde_json::Value;

use crate::cx::ToolCx;
use crate::error::{AgentError, ToolError};
use crate::provider::ToolDef;
use crate::tool::{ToolDyn, ToolSet};

/// An MCP server definition.
///
/// Matches the `mcpServers` JSON config schema used by Claude Code / Claude
/// Desktop. `${VAR}` / `${VAR:-default}` placeholders in string fields are
/// expanded from the environment via [`McpServer::expand_env`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpServer {
    /// A local process speaking MCP over stdio (client mode only).
    Stdio {
        /// Executable to run.
        command: String,
        /// Command-line arguments.
        #[serde(default)]
        args: Vec<String>,
        /// Environment variables for the child process.
        #[serde(default)]
        env: HashMap<String, String>,
    },
    /// A remote streamable-HTTP server.
    Http {
        /// Server URL.
        url: String,
        /// Extra request headers.
        #[serde(default)]
        headers: HashMap<String, String>,
    },
    /// A remote SSE server (deprecated transport).
    Sse {
        /// Server URL.
        url: String,
        /// Extra request headers.
        #[serde(default)]
        headers: HashMap<String, String>,
    },
}

impl McpServer {
    /// Expands `${VAR}` / `${VAR:-default}` placeholders in string fields.
    #[must_use]
    pub fn expand_env(self) -> Self {
        match self {
            Self::Stdio { command, args, env } => Self::Stdio {
                command: expand(&command),
                args: args.iter().map(|a| expand(a)).collect(),
                env: env.into_iter().map(|(k, v)| (k, expand(&v))).collect(),
            },
            Self::Http { url, headers } => Self::Http {
                url: expand(&url),
                headers: headers.into_iter().map(|(k, v)| (k, expand(&v))).collect(),
            },
            Self::Sse { url, headers } => Self::Sse {
                url: expand(&url),
                headers: headers.into_iter().map(|(k, v)| (k, expand(&v))).collect(),
            },
        }
    }
}

/// The `mcpServers` map from an MCP JSON config.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct McpConfig {
    /// Servers keyed by name.
    #[serde(rename = "mcpServers")]
    pub servers: HashMap<String, McpServer>,
}

impl McpConfig {
    /// Parses an MCP config from JSON text.
    ///
    /// # Errors
    /// Returns [`AgentError::Mcp`] if the JSON is malformed.
    pub fn from_json(text: &str) -> Result<Self, AgentError> {
        serde_json::from_str(text).map_err(|e| AgentError::Mcp(format!("invalid mcp config: {e}")))
    }

    /// Expands environment placeholders in every server definition.
    #[must_use]
    pub fn expand_env(self) -> Self {
        Self {
            servers: self
                .servers
                .into_iter()
                .map(|(k, v)| (k, v.expand_env()))
                .collect(),
        }
    }
}

/// Expands a single `${VAR}` or `${VAR:-default}` occurrence-by-occurrence.
fn expand(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        if let Some(close) = after.find('}') {
            let expr = &after[..close];
            let (name, default) = expr
                .split_once(":-")
                .map_or((expr, None), |(n, d)| (n, Some(d)));
            let value = std::env::var(name).unwrap_or_else(|_| default.unwrap_or("").to_owned());
            out.push_str(&value);
            rest = &after[close + 1..];
        } else {
            out.push_str("${");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// A tool advertised by an MCP server.
#[derive(Debug, Clone)]
pub struct McpTool {
    /// Tool name as known to the server.
    pub name: String,
    /// Tool description.
    pub description: String,
    /// JSON Schema for the tool's arguments.
    pub input_schema: Value,
}

/// A connected MCP client. Implement this over any MCP transport (`rmcp`, a
/// custom stdio/HTTP client, …); the adapter below turns it into registry tools.
pub trait McpTransport: Send + Sync + 'static {
    /// Lists the server's tools.
    fn list_tools(&self) -> BoxFuture<'_, Result<Vec<McpTool>, AgentError>>;
    /// Calls a tool by its server-side name with JSON arguments.
    fn call_tool<'a>(
        &'a self,
        name: &'a str,
        args: Value,
    ) -> BoxFuture<'a, Result<Value, AgentError>>;
}

/// Adapts a connected [`McpTransport`] into a [`ToolSet`]: each MCP tool becomes
/// a [`ToolDyn`] named `mcp__<server>__<tool>`.
pub struct McpToolSet<T> {
    server: String,
    transport: Arc<T>,
    tools: Vec<McpTool>,
}

impl<T: McpTransport> McpToolSet<T> {
    /// Connects: lists the transport's tools and prepares the adapter.
    ///
    /// # Errors
    /// Propagates transport errors from `list_tools`.
    pub async fn connect(server: impl Into<String>, transport: T) -> Result<Self, AgentError> {
        let transport = Arc::new(transport);
        let tools = transport.list_tools().await?;
        Ok(Self {
            server: server.into(),
            transport,
            tools,
        })
    }

    /// The namespaced tool names this set exposes.
    #[must_use]
    pub fn tool_names(&self) -> Vec<String> {
        self.tools
            .iter()
            .map(|t| format!("mcp__{}__{}", self.server, t.name))
            .collect()
    }
}

impl<Ctx: Send + Sync + 'static, T: McpTransport> ToolSet<Ctx> for McpToolSet<T> {
    fn into_tools(self) -> Vec<Arc<dyn ToolDyn<Ctx>>> {
        self.tools
            .into_iter()
            .map(|tool| {
                let def = ToolDef::new(
                    format!("mcp__{}__{}", self.server, tool.name),
                    tool.description,
                    tool.input_schema,
                );
                Arc::new(McpToolDyn {
                    def,
                    remote_name: tool.name,
                    transport: Arc::clone(&self.transport),
                }) as Arc<dyn ToolDyn<Ctx>>
            })
            .collect()
    }
}

struct McpToolDyn<T> {
    def: ToolDef,
    remote_name: String,
    transport: Arc<T>,
}

impl<Ctx, T: McpTransport> ToolDyn<Ctx> for McpToolDyn<T> {
    fn name(&self) -> &str {
        &self.def.name
    }

    fn def(&self) -> ToolDef {
        self.def.clone()
    }

    fn call_json<'a>(
        &'a self,
        _cx: &'a ToolCx<Ctx>,
        args: Value,
    ) -> BoxFuture<'a, Result<Value, ToolError>> {
        Box::pin(async move {
            self.transport
                .call_tool(&self.remote_name, args)
                .await
                .map_err(|e| ToolError::new(e.to_string()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_stdio_and_http_config() {
        let cfg = McpConfig::from_json(
            r#"{ "mcpServers": {
                "fs": { "type": "stdio", "command": "mcp-fs", "args": ["--root","/tmp"] },
                "gh": { "type": "http", "url": "https://mcp.example.com", "headers": { "Authorization": "Bearer x" } }
            } }"#,
        )
        .expect("parse");
        assert_eq!(cfg.servers.len(), 2);
        assert!(matches!(cfg.servers["fs"], McpServer::Stdio { .. }));
        assert!(matches!(cfg.servers["gh"], McpServer::Http { .. }));
    }

    #[test]
    fn expands_env_default_and_unset_placeholders() {
        let mut headers = HashMap::new();
        headers.insert("X".to_owned(), "v-${STAKIT_UNSET_VAR}".to_owned());
        let server = McpServer::Http {
            url: "https://x/${STAKIT_UNSET_VAR:-fallback}".into(),
            headers,
        }
        .expand_env();
        let McpServer::Http { url, headers } = server else {
            panic!("expected http");
        };
        assert_eq!(url, "https://x/fallback"); // `:-default` used when unset
        assert_eq!(headers["X"], "v-"); // bare unset expands to empty
    }

    struct MockTransport;

    impl McpTransport for MockTransport {
        fn list_tools(&self) -> BoxFuture<'_, Result<Vec<McpTool>, AgentError>> {
            Box::pin(async {
                Ok(vec![McpTool {
                    name: "search".into(),
                    description: "search the web".into(),
                    input_schema: json!({ "type": "object" }),
                }])
            })
        }
        fn call_tool<'a>(
            &'a self,
            name: &'a str,
            _args: Value,
        ) -> BoxFuture<'a, Result<Value, AgentError>> {
            Box::pin(async move { Ok(json!({ "called": name })) })
        }
    }

    #[tokio::test]
    async fn tool_set_namespaces_and_proxies_calls() {
        let set = McpToolSet::connect("web", MockTransport)
            .await
            .expect("connect");
        assert_eq!(set.tool_names(), vec!["mcp__web__search"]);
        let tools: Vec<Arc<dyn ToolDyn<()>>> = set.into_tools();
        assert_eq!(tools[0].def().name, "mcp__web__search");
        let cx = ToolCx::new(());
        let out = tools[0].call_json(&cx, json!({})).await.expect("call");
        assert_eq!(out, json!({ "called": "search" }));
    }
}
