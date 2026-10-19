//! `with_endpoint` sends to the exact URL given; the default keeps `{base}/v1/...`.

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::thread;

use crate::{ChatRequest, ClaudeClient, Message, OpenAiClient, Provider};

/// Accepts one request, records its full head, answers 500.
fn capture_request_head() -> (String, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let handle = thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept");
        let mut buffer = [0; 8192];
        let count = socket.read(&mut buffer).expect("read");
        let _ =
            socket.write_all(b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 2\r\n\r\n{}");
        String::from_utf8_lossy(&buffer[..count]).into_owned()
    });
    (url, handle)
}

/// The request line the capture server received.
fn request_line(server: thread::JoinHandle<String>) -> String {
    let head = server.join().expect("server");
    head.lines().next().unwrap_or_default().to_owned()
}

/// Whether the captured request carried the probing client's header.
fn was_probed(server: thread::JoinHandle<String>) -> bool {
    server
        .join()
        .expect("server")
        .to_ascii_lowercase()
        .contains("x-probe: 1")
}

async fn send_once(provider: Box<dyn Provider>) {
    let mut request = ChatRequest::new(provider.model_id());
    request.messages = vec![Message::user("hi")];
    // The fixture answers 500; only the request line matters here.
    let _ = provider.complete(request).await;
}

#[tokio::test]
async fn openai_endpoint_override_is_used_verbatim() {
    let (url, server) = capture_request_head();
    let client =
        OpenAiClient::new("k").with_endpoint(format!("{url}/v1beta/openai/chat/completions"));
    send_once(Box::new(client.model("gemini-x"))).await;
    assert_eq!(
        request_line(server),
        "POST /v1beta/openai/chat/completions HTTP/1.1"
    );
}

#[tokio::test]
async fn claude_endpoint_override_is_used_verbatim() {
    let (url, server) = capture_request_head();
    let client = ClaudeClient::new("k").with_endpoint(format!("{url}/proxy/messages"));
    send_once(Box::new(client.model("claude-x"))).await;
    assert_eq!(request_line(server), "POST /proxy/messages HTTP/1.1");
}

#[tokio::test]
async fn openai_default_path_is_unchanged() {
    let (url, server) = capture_request_head();
    let client = OpenAiClient::new("k").with_base_url(url);
    send_once(Box::new(client.model("gpt-x"))).await;
    assert_eq!(request_line(server), "POST /v1/chat/completions HTTP/1.1");
}

fn probing_client() -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("x-probe", reqwest::header::HeaderValue::from_static("1"));
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .expect("client")
}

#[tokio::test]
async fn openai_uses_the_given_http_client() {
    let (url, server) = capture_request_head();
    let client = OpenAiClient::new("k")
        .with_base_url(url)
        .with_http_client(probing_client());
    send_once(Box::new(client.model("gpt-x"))).await;
    assert!(was_probed(server));
}

#[tokio::test]
async fn claude_uses_the_given_http_client() {
    let (url, server) = capture_request_head();
    let client = ClaudeClient::new("k")
        .with_base_url(url)
        .with_http_client(probing_client());
    send_once(Box::new(client.model("claude-x"))).await;
    assert!(was_probed(server));
}
