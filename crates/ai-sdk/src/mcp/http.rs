//! Streamable-HTTP MCP client (2025-03-26): JSON-RPC over POST, answered as
//! JSON or as a `text/event-stream` carrying the response.
//!
//! The network is the host's: it implements [`McpHttpClient`] over whatever
//! HTTP stack it trusts (an SSRF-safe client, a proxy), enforces timeouts and
//! caps the body. This module owns only the protocol: `initialize`, the
//! session id, `tools/list` paging and `tools/call`.

#[cfg(test)]
#[path = "http_test.rs"]
mod http_test;

use std::sync::atomic::{AtomicU64, Ordering};

use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::{McpTool, McpTransport};
use crate::error::AgentError;

/// Largest response body accepted, in bytes.
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
/// Most `tools/list` pages followed.
const MAX_PAGES: usize = 5;
/// The protocol version asked for.
const PROTOCOL_VERSION: &str = "2025-03-26";

/// One POST to the server.
#[derive(Debug, Clone)]
pub struct McpHttpRequest {
    /// The server URL.
    pub url: String,
    /// Request headers (auth, `Mcp-Session-Id`, `Accept`, `Content-Type`).
    pub headers: Vec<(String, String)>,
    /// The JSON-RPC body.
    pub body: Vec<u8>,
}

/// The server's answer.
#[derive(Debug, Clone)]
pub struct McpHttpResponse {
    /// HTTP status.
    pub status: u16,
    /// The `Content-Type` header, when present.
    pub content_type: Option<String>,
    /// The `Mcp-Session-Id` response header, when present.
    pub session_id: Option<String>,
    /// The body (at most [`MAX_RESPONSE_BYTES`]).
    pub body: Vec<u8>,
}

/// Sends one POST. The host enforces its network policy here.
pub trait McpHttpClient: Send + Sync + 'static {
    /// POST `request`; fail with [`AgentError::Mcp`] on transport errors.
    fn post(&self, request: McpHttpRequest) -> BoxFuture<'_, Result<McpHttpResponse, AgentError>>;
}

struct Session {
    is_ready: bool,
    id: Option<String>,
}

/// An MCP server reached over streamable HTTP.
pub struct McpHttp<C> {
    client: C,
    url: String,
    headers: Vec<(String, String)>,
    session: Mutex<Session>,
    next_id: AtomicU64,
}

fn mcp_error(message: impl Into<String>) -> AgentError {
    AgentError::Mcp(message.into())
}

/// The JSON-RPC message with `id` out of an event-stream body.
fn from_event_stream(body: &[u8], id: u64) -> Result<Value, AgentError> {
    let text = std::str::from_utf8(body).map_err(|_| mcp_error("response is not UTF-8"))?;
    let normalized = text.replace("\r\n", "\n");
    for event in normalized.split("\n\n") {
        let data: Vec<&str> = event
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(|line| line.strip_prefix(' ').unwrap_or(line))
            .collect();
        if data.is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&data.join("\n")) else {
            continue;
        };
        if message.get("id").and_then(Value::as_u64) == Some(id) {
            return Ok(message);
        }
    }
    Err(mcp_error("the stream ended without an answer"))
}

/// The `result` of a JSON-RPC answer, or its error.
fn result_of(message: &Value) -> Result<Value, AgentError> {
    if let Some(error) = message.get("error") {
        let text = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the server returned an error");
        return Err(mcp_error(text.chars().take(200).collect::<String>()));
    }
    message
        .get("result")
        .cloned()
        .ok_or_else(|| mcp_error("the answer has no result"))
}

impl<C: McpHttpClient> McpHttp<C> {
    /// A client for `url` sending `headers` with every request.
    pub fn new(client: C, url: impl Into<String>, headers: Vec<(String, String)>) -> Self {
        Self {
            client,
            url: url.into(),
            headers,
            session: Mutex::new(Session {
                is_ready: false,
                id: None,
            }),
            next_id: AtomicU64::new(1),
        }
    }

    fn request(&self, session_id: Option<&str>, body: &Value) -> McpHttpRequest {
        let mut headers = self.headers.clone();
        headers.push(("Content-Type".to_owned(), "application/json".to_owned()));
        headers.push((
            "Accept".to_owned(),
            "application/json, text/event-stream".to_owned(),
        ));
        if let Some(id) = session_id {
            headers.push(("Mcp-Session-Id".to_owned(), id.to_owned()));
        }
        McpHttpRequest {
            url: self.url.clone(),
            headers,
            body: body.to_string().into_bytes(),
        }
    }

    async fn send(
        &self,
        session_id: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<(Value, Option<String>), AgentError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let response = self.client.post(self.request(session_id, &body)).await?;
        if response.body.len() > MAX_RESPONSE_BYTES {
            return Err(mcp_error("the response is too large"));
        }
        if !(200..300).contains(&response.status) {
            return Err(mcp_error(format!(
                "the server answered {}",
                response.status
            )));
        }
        let is_stream = response
            .content_type
            .as_deref()
            .is_some_and(|kind| kind.to_ascii_lowercase().starts_with("text/event-stream"));
        let message = if is_stream {
            from_event_stream(&response.body, id)?
        } else {
            let message: Value = serde_json::from_slice(&response.body)
                .map_err(|_| mcp_error("the answer is not JSON"))?;
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                return Err(mcp_error("the answer is for another request"));
            }
            message
        };
        Ok((result_of(&message)?, response.session_id))
    }

    /// `initialize` once, remember the session, and say `initialized`.
    async fn session_id(&self) -> Result<Option<String>, AgentError> {
        let session = &mut *self.session.lock().await;
        if session.is_ready {
            return Ok(session.id.clone());
        }
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "elios", "version": "1" },
        });
        let (_, id) = self.send(None, "initialize", params).await?;
        let notice = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        let reply = self
            .client
            .post(self.request(id.as_deref(), &notice))
            .await?;
        if !(200..300).contains(&reply.status) {
            return Err(mcp_error("the server refused to start a session"));
        }
        session.is_ready = true;
        session.id.clone_from(&id);
        Ok(id)
    }

    async fn list(&self) -> Result<Vec<McpTool>, AgentError> {
        let session_id = self.session_id().await?;
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let params = cursor
                .as_deref()
                .map_or_else(|| json!({}), |cursor| json!({ "cursor": cursor }));
            let (page, _) = self
                .send(session_id.as_deref(), "tools/list", params)
                .await?;
            for tool in page
                .get("tools")
                .and_then(Value::as_array)
                .map_or(&[][..], Vec::as_slice)
            {
                let Some(name) = tool.get("name").and_then(Value::as_str) else {
                    continue;
                };
                tools.push(McpTool {
                    name: name.to_owned(),
                    description: tool
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    input_schema: tool
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({ "type": "object" })),
                    read_only: tool
                        .get("annotations")
                        .and_then(|a| a.get("readOnlyHint"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    destructive: tool
                        .get("annotations")
                        .and_then(|a| a.get("destructiveHint"))
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                });
            }
            cursor = page
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        Ok(tools)
    }

    async fn call(&self, name: &str, args: Value) -> Result<Value, AgentError> {
        let session_id = self.session_id().await?;
        let params = json!({ "name": name, "arguments": args });
        let (result, _) = self
            .send(session_id.as_deref(), "tools/call", params)
            .await?;
        Ok(result)
    }
}

impl<C: McpHttpClient> McpTransport for McpHttp<C> {
    fn list_tools(&self) -> BoxFuture<'_, Result<Vec<McpTool>, AgentError>> {
        Box::pin(self.list())
    }

    fn call_tool<'a>(
        &'a self,
        name: &'a str,
        args: Value,
    ) -> BoxFuture<'a, Result<Value, AgentError>> {
        Box::pin(self.call(name, args))
    }
}
