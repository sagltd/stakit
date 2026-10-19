//! One bounded loopback HTTP exchange, shared by adapter contract probes.
use serde_json::Value;
use std::{
    io::Write,
    net::TcpListener,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

#[path = "../support/http_request.rs"]
mod http_request;
use http_request::read_request;

pub(super) struct HttpFixture {
    pub(super) base_url: String,
    received: mpsc::Receiver<Value>,
    task: thread::JoinHandle<()>,
}
impl HttpFixture {
    pub(super) fn finish(self) -> Value {
        let request = self
            .received
            .recv_timeout(Duration::from_secs(6))
            .expect("fixture request");
        self.task.join().expect("fixture worker");
        request
    }
}

pub(super) fn http(status: u16, content_type: &str, body: Vec<u8>) -> HttpFixture {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let base_url = format!("http://{}", listener.local_addr().expect("fixture address"));
    let headers = format!(
        "HTTP/1.1 {status} Fixture\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let (sender, received) = mpsc::channel();
    let task = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(_) => return,
            }
        };
        socket
            .set_nonblocking(false)
            .expect("blocking accepted socket");
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read deadline");
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .expect("write deadline");
        let (bytes, header_end) = read_request(&mut socket, 1024 * 1024);
        let request = serde_json::from_slice(&bytes[header_end..]).expect("request JSON");
        sender.send(request).expect("request receiver");
        socket
            .write_all(headers.as_bytes())
            .expect("response headers");
        socket.write_all(&body).expect("response body");
    });
    HttpFixture {
        base_url,
        received,
        task,
    }
}

pub(super) fn sse(events: &[Value]) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut body = String::new();
    for event in events {
        write!(&mut body, "data: {event}\n\n").expect("write fixture SSE");
    }
    body.into_bytes()
}
