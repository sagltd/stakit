//! Bound real provider HTTP reads before buffering the whole response.
#![cfg(all(feature = "claude", feature = "openai"))]

#[path = "support/http_request.rs"]
mod http_request;
#[path = "support/http_socket.rs"]
mod http_socket;

use std::io::Write as _;
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use futures::StreamExt as _;
use serde_json::json;
use stakit_ai_sdk::{
    ChatRequest, ClaudeClient, OpenAiClient, Provider, ProviderError, StreamEvent,
};

struct Fixture {
    url: String,
    release: mpsc::Sender<()>,
    task: thread::JoinHandle<()>,
}

impl Fixture {
    fn finish(self) {
        let _ = self.release.send(());
        self.task.join().expect("fixture worker");
    }
}

/// Omits Content-Length; optionally holds the body open to prove early refusal.
fn response(body: Vec<u8>, status: u16, chunked: bool, complete: bool) -> Fixture {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let url = format!("http://{}", listener.local_addr().expect("fixture address"));
    let (release, receive) = mpsc::channel();
    let task = thread::spawn(move || {
        let mut socket = http_socket::accept(&listener);
        http_request::read_request(&mut socket, 1024 * 1024);
        let framing = if chunked {
            "Transfer-Encoding: chunked\r\n"
        } else {
            ""
        };
        write!(socket, "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nRetry-After: 7\r\n{framing}Connection: close\r\n\r\n").expect("response headers");
        for part in body.chunks(4096) {
            let sent = if chunked {
                write!(socket, "{:x}\r\n", part.len())
                    .and_then(|()| socket.write_all(part))
                    .and_then(|()| socket.write_all(b"\r\n"))
            } else {
                socket.write_all(part)
            };
            if let Err(error) = sent {
                assert!(
                    matches!(
                        error.kind(),
                        std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                    ),
                    "response write: {error}"
                );
                break;
            }
        }
        if complete {
            if chunked {
                let _ = socket.write_all(b"0\r\n\r\n");
            }
        } else {
            let _ = receive.recv_timeout(Duration::from_secs(5));
        }
    });
    Fixture { url, release, task }
}

fn provider(claude: bool, url: &str, limit: Option<usize>) -> Box<dyn Provider> {
    let http = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("HTTP client");
    macro_rules! configured {
        ($client:expr, $model:literal) => {{
            let client = $client.with_base_url(url).with_http_client(http);
            let client = match limit {
                Some(limit) => client.with_response_byte_limit(limit),
                None => client,
            };
            Box::new(client.model($model))
        }};
    }
    if claude {
        configured!(ClaudeClient::new("fixture-not-a-key"), "claude-fixture")
    } else {
        configured!(OpenAiClient::new("fixture-not-a-key"), "openai-fixture")
    }
}

async fn rejects_unfinished_response(claude: bool, chunked: bool) {
    let fixture = response(vec![b' '; 8 * 1024 * 1024 + 1], 200, chunked, false);
    let provider = provider(claude, &fixture.url, None);
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        provider.complete(ChatRequest::new(provider.model_id())),
    )
    .await;
    fixture.finish();
    let error = result
        .expect("must reject before EOF")
        .expect_err("oversized body");
    assert!(
        matches!(error, ProviderError::Decode { body, .. } if body.is_empty()),
        "size error must not retain payload"
    );
}

#[tokio::test]
async fn openai_rejects_oversized_response_before_eof() {
    for chunked in [false, true] {
        rejects_unfinished_response(false, chunked).await;
    }
}

#[tokio::test]
async fn claude_rejects_oversized_response_before_eof() {
    for chunked in [false, true] {
        rejects_unfinished_response(true, chunked).await;
    }
}

fn completion(claude: bool) -> Vec<u8> {
    let value = if claude {
        json!({"content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":1}})
    } else {
        json!({"choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}})
    };
    value.to_string().into_bytes()
}

#[tokio::test]
async fn configured_completion_limit_accepts_exact_boundary_and_rejects_excess() {
    for claude in [false, true] {
        let body = completion(claude);
        for limit in [0, body.len() - 1, body.len()] {
            let fixture = response(body.clone(), 200, true, true);
            let provider = provider(claude, &fixture.url, Some(limit));
            let result = provider
                .complete(ChatRequest::new(provider.model_id()))
                .await;
            fixture.finish();
            if limit == body.len() {
                assert_eq!(result.expect("exact boundary").usage.output_tokens, 1);
            } else {
                assert!(
                    matches!(result, Err(ProviderError::Decode { body, .. }) if body.is_empty())
                );
            }
        }
    }
}

#[tokio::test]
async fn configured_limit_rejects_incremental_reads_without_eof() {
    for claude in [false, true] {
        for chunked in [false, true] {
            let fixture = response(vec![b' '; 1025], 200, chunked, false);
            let provider = provider(claude, &fixture.url, Some(1024));
            let result = tokio::time::timeout(
                Duration::from_secs(3),
                provider.complete(ChatRequest::new(provider.model_id())),
            )
            .await;
            fixture.finish();
            assert!(
                matches!(result.expect("must reject before EOF"), Err(ProviderError::Decode { body, .. }) if body.is_empty())
            );
        }
    }
}

#[tokio::test]
async fn oversized_errors_preserve_status_and_retry_hint_without_reading_to_eof() {
    for claude in [false, true] {
        for stream in [false, true] {
            for limit in [32, 128 * 1024] {
                let fixture = response(vec![b'x'; limit.min(64 * 1024) + 1], 429, true, false);
                let provider = provider(claude, &fixture.url, Some(limit));
                let request = ChatRequest::new(provider.model_id());
                let result = tokio::time::timeout(Duration::from_secs(3), async {
                    if stream {
                        provider.stream(request).await.map(|_| ())
                    } else {
                        provider.complete(request).await.map(|_| ())
                    }
                })
                .await;
                fixture.finish();
                match result
                    .expect("error read must stop before EOF")
                    .expect_err("429")
                {
                    ProviderError::Api {
                        status,
                        retry_after,
                        message,
                        ..
                    } => {
                        assert_eq!((status, retry_after), (429, Some(Duration::from_secs(7))));
                        assert_eq!(message, "provider error: 429 api_error");
                    }
                    error => panic!("expected API status, got {error:?}"),
                }
            }
        }
    }
}

#[tokio::test]
async fn malformed_json_error_does_not_retain_response_content() {
    for claude in [false, true] {
        let fixture = response(b"not-json SECRET-FIXTURE".to_vec(), 200, false, true);
        let provider = provider(claude, &fixture.url, None);
        let result = provider
            .complete(ChatRequest::new(provider.model_id()))
            .await;
        fixture.finish();
        let error = result.expect_err("invalid JSON");
        assert!(!format!("{error:?}").contains("SECRET-FIXTURE"));
        assert!(matches!(error, ProviderError::Decode { body, .. } if body.is_empty()));
    }
}

#[tokio::test]
async fn success_stream_uses_frame_limits_instead_of_total_http_body_limit() {
    for claude in [false, true] {
        let body = if claude {
            concat!(
                "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":0}}\n\n",
                "data: {\"type\":\"message_stop\"}\n\n"
            )
        } else {
            concat!(
                "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":0}}\n\n",
                "data: [DONE]\n\n"
            )
        };
        let fixture = response(body.as_bytes().to_vec(), 200, true, true);
        let provider = provider(claude, &fixture.url, Some(1));
        let events = provider
            .stream(ChatRequest::new(provider.model_id()))
            .await
            .expect("stream")
            .collect::<Vec<_>>()
            .await;
        fixture.finish();
        assert!(
            matches!(events.last(), Some(Ok(StreamEvent::End { .. }))),
            "{events:?}"
        );
    }
}
