use std::sync::Mutex;

use futures::future::BoxFuture;
use serde_json::{Value, json};

use super::{MAX_RESPONSE_BYTES, McpHttp, McpHttpClient, McpHttpRequest, McpHttpResponse};
use crate::error::AgentError;
use crate::mcp::McpTransport;

type Seen = (String, Vec<(String, String)>);

/// A scripted server: answers by method, records each request.
struct Fake {
    seen: Mutex<Vec<Seen>>,
    as_stream: bool,
    oversize: bool,
    pages: bool,
}

fn reply(body: &Value, stream: bool, session: Option<&str>) -> McpHttpResponse {
    let (content_type, body) = if stream {
        (
            "text/event-stream",
            format!("event: message\r\ndata: {body}\r\n\r\n").into_bytes(),
        )
    } else {
        ("application/json", body.to_string().into_bytes())
    };
    McpHttpResponse {
        status: 200,
        content_type: Some(content_type.to_owned()),
        session_id: session.map(str::to_owned),
        body,
    }
}

impl McpHttpClient for Fake {
    fn post(&self, request: McpHttpRequest) -> BoxFuture<'_, Result<McpHttpResponse, AgentError>> {
        Box::pin(async move {
            let body: Value = serde_json::from_slice(&request.body).expect("json");
            let method = body["method"].as_str().unwrap_or_default().to_owned();
            self.seen
                .lock()
                .expect("lock")
                .push((method.clone(), request.headers.clone()));
            let id = body["id"].clone();
            if self.oversize {
                return Ok(McpHttpResponse {
                    status: 200,
                    content_type: None,
                    session_id: None,
                    body: vec![b' '; MAX_RESPONSE_BYTES + 1],
                });
            }
            let answer = match method.as_str() {
                "initialize" => {
                    json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":"2025-03-26"}})
                }
                "notifications/initialized" => {
                    return Ok(McpHttpResponse {
                        status: 202,
                        content_type: None,
                        session_id: None,
                        body: Vec::new(),
                    });
                }
                "tools/list" if self.pages && body["params"].get("cursor").is_none() => {
                    json!({"jsonrpc":"2.0","id":id,"result":{
                        "tools":[{"name":"a","inputSchema":{"type":"object"}}],"nextCursor":"2"}})
                }
                "tools/list" => json!({"jsonrpc":"2.0","id":id,"result":{"tools":[{
                    "name":"b","description":"d","inputSchema":{"type":"object"},
                    "annotations":{"readOnlyHint":true,"destructiveHint":false}}]}}),
                "tools/call" if body["params"]["name"] == "boom" => {
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":-1,"message":"nope"}})
                }
                "tools/call" => json!({"jsonrpc":"2.0","id":id,"result":{
                    "content":[{"type":"text","text":"hi"}]}}),
                _ => json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"?"}}),
            };
            Ok(reply(&answer, self.as_stream, Some("sess-1")))
        })
    }
}

fn fake() -> Fake {
    Fake {
        seen: Mutex::new(Vec::new()),
        as_stream: false,
        oversize: false,
        pages: false,
    }
}

fn client(fake: Fake) -> McpHttp<Fake> {
    McpHttp::new(
        fake,
        "https://mcp.example.com/mcp",
        vec![("Authorization".to_owned(), "Bearer x".to_owned())],
    )
}

#[tokio::test]
async fn starts_a_session_once_and_sends_its_id_and_auth() {
    let http = client(fake());
    http.list_tools().await.expect("list");
    http.call_tool("b", json!({})).await.expect("call");
    let seen = http.client.seen.lock().expect("lock").clone();
    let methods: Vec<&str> = seen.iter().map(|(method, _)| method.as_str()).collect();
    assert_eq!(
        methods,
        [
            "initialize",
            "notifications/initialized",
            "tools/list",
            "tools/call"
        ]
    );
    let call_headers = &seen[3].1;
    assert!(call_headers.contains(&("Mcp-Session-Id".to_owned(), "sess-1".to_owned())));
    assert!(call_headers.contains(&("Authorization".to_owned(), "Bearer x".to_owned())));
}

#[tokio::test]
async fn reads_tools_with_their_hints_and_follows_pages() {
    let http = client(Fake {
        pages: true,
        ..fake()
    });
    let tools = http.list_tools().await.expect("list");
    assert_eq!(
        tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert!(
        !tools[0].read_only && tools[0].destructive,
        "no hints = assume the worst"
    );
    assert!(tools[1].read_only && !tools[1].destructive);
}

#[tokio::test]
async fn an_event_stream_answer_is_read_by_id() {
    let http = client(Fake {
        as_stream: true,
        ..fake()
    });
    let result = http.call_tool("b", json!({})).await.expect("call");
    assert_eq!(result["content"][0]["text"], "hi");
}

#[tokio::test]
async fn a_json_rpc_error_becomes_an_mcp_error() {
    let http = client(fake());
    let error = http.call_tool("boom", json!({})).await.expect_err("error");
    assert!(error.to_string().contains("nope"));
}

#[tokio::test]
async fn an_oversize_response_is_refused() {
    let http = client(Fake {
        oversize: true,
        ..fake()
    });
    assert!(http.list_tools().await.is_err());
}

#[tokio::test]
async fn a_malformed_answer_is_an_error_not_a_panic() {
    struct Garbage;
    impl McpHttpClient for Garbage {
        fn post(&self, _: McpHttpRequest) -> BoxFuture<'_, Result<McpHttpResponse, AgentError>> {
            Box::pin(async {
                Ok(McpHttpResponse {
                    status: 200,
                    content_type: Some("application/json".to_owned()),
                    session_id: None,
                    body: b"{not json".to_vec(),
                })
            })
        }
    }
    let http = McpHttp::new(Garbage, "https://x/mcp", Vec::new());
    assert!(http.list_tools().await.is_err());
}

#[tokio::test]
async fn a_non_success_status_is_an_error() {
    struct Denied;
    impl McpHttpClient for Denied {
        fn post(&self, _: McpHttpRequest) -> BoxFuture<'_, Result<McpHttpResponse, AgentError>> {
            Box::pin(async {
                Ok(McpHttpResponse {
                    status: 401,
                    content_type: None,
                    session_id: None,
                    body: Vec::new(),
                })
            })
        }
    }
    let http = McpHttp::new(Denied, "https://x/mcp", Vec::new());
    let error = http.list_tools().await.expect_err("401");
    assert!(error.to_string().contains("401"));
}
