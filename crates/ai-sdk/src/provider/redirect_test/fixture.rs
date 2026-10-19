//! Two bounded origins. Retain only exposure flags, never request material.

use std::io::Write as _;
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

pub(super) const KEY: &str = "synthetic-only-key";
pub(super) const BODY: &str = "synthetic private body sentinel";

#[derive(Default)]
struct Observed {
    requests: AtomicUsize,
    key: AtomicBool,
    body: AtomicBool,
    post: AtomicBool,
}

pub(super) struct Snapshot {
    pub requests: usize,
    pub has_key: bool,
    pub has_body: bool,
    pub is_post: bool,
}

#[path = "../../../tests/support/http_request.rs"]
mod http_request;
use http_request::read_request;

struct Peer {
    url: String,
    observed: Arc<Observed>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Peer {
    fn start(status: u16, location: Option<&str>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback origin");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let url = format!("http://{}", listener.local_addr().expect("origin address"));
        let body = r#"{"content":[],"stop_reason":"end_turn","choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"input_tokens":1,"output_tokens":1,"prompt_tokens":1,"completion_tokens":1}}"#;
        let location = location.map_or_else(String::new, |url| format!("Location: {url}/sink\r\n"));
        let response = format!(
            "HTTP/1.1 {status} Fixture\r\n{location}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let observed = Arc::new(Observed::default());
        let stop = Arc::new(AtomicBool::new(false));
        let received = Arc::clone(&observed);
        let stopped = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(6);
            while !stopped.load(Ordering::Acquire) && Instant::now() < deadline {
                let mut socket = match listener.accept() {
                    Ok((socket, _)) => socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("fixture accept failed: {error}"),
                };
                socket
                    .set_nonblocking(false)
                    .expect("blocking accepted socket");
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .expect("read deadline");
                socket
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .expect("write deadline");
                let (bytes, end) = read_request(&mut socket, 64 * 1024);
                let header = std::str::from_utf8(&bytes[..end]).expect("UTF-8 headers");
                received.requests.fetch_add(1, Ordering::SeqCst);
                received
                    .post
                    .store(header.starts_with("POST "), Ordering::SeqCst);
                received.key.store(
                    header.lines().any(|line| {
                        line.split_once(':').is_some_and(|(name, value)| {
                            name.eq_ignore_ascii_case("x-api-key") && value.trim() == KEY
                        })
                    }),
                    Ordering::SeqCst,
                );
                received.body.store(
                    bytes[end..]
                        .windows(BODY.len())
                        .any(|part| part == BODY.as_bytes()),
                    Ordering::SeqCst,
                );
                socket
                    .write_all(response.as_bytes())
                    .expect("fixture response");
            }
        });
        Self {
            url,
            observed,
            stop,
            worker: Some(worker),
        }
    }

    fn finish(&mut self) -> Snapshot {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("fixture worker");
        }
        Snapshot {
            requests: self.observed.requests.load(Ordering::SeqCst),
            has_key: self.observed.key.load(Ordering::SeqCst),
            has_body: self.observed.body.load(Ordering::SeqCst),
            is_post: self.observed.post.load(Ordering::SeqCst),
        }
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

pub(super) struct Fixture {
    pub url: String,
    origin: Peer,
    target: Peer,
}

impl Fixture {
    pub(super) fn start(status: u16) -> Self {
        let target = Peer::start(200, None);
        let origin = Peer::start(status, Some(&target.url));
        Self {
            url: origin.url.clone(),
            origin,
            target,
        }
    }

    pub(super) fn finish(mut self) -> (Snapshot, Snapshot) {
        (self.origin.finish(), self.target.finish())
    }
}
