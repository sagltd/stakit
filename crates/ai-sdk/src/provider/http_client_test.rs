//! Count real HTTP/2 `REFUSED_STREAM` retries against a bounded loopback peer.
use super::http_client_builder;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

fn refuse_streams(mut socket: TcpStream) -> usize {
    // Accepted sockets may inherit nonblocking mode from the listener.
    socket
        .set_nonblocking(false)
        .expect("blocking accepted socket");
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("read deadline");
    socket
        .set_write_timeout(Some(Duration::from_secs(3)))
        .expect("write deadline");
    let mut preface = [0; 24];
    socket.read_exact(&mut preface).expect("HTTP/2 preface");
    assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
    socket
        .write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0])
        .expect("server SETTINGS");
    let mut requests = 0;
    let mut frames = 0;
    loop {
        let mut header = [0; 9];
        if let Err(error) = socket.read_exact(&mut header) {
            eprintln!(
                "HTTP/2 peer frame read ended: {:?}; frames={frames}; requests={requests}",
                error.kind()
            );
            assert!(matches!(
                error.kind(),
                std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
            ));
            return requests;
        }
        let length =
            (usize::from(header[0]) << 16) | (usize::from(header[1]) << 8) | usize::from(header[2]);
        assert!(length <= 64 * 1024, "fixture frame exceeds bound");
        if frames < 8 {
            eprintln!(
                "HTTP/2 peer frame: type={} flags={} length={length} stream={}",
                header[3],
                header[4],
                u32::from_be_bytes([header[5], header[6], header[7], header[8]])
            );
        }
        frames += 1;
        let mut payload = vec![0; length];
        socket.read_exact(&mut payload).expect("frame payload");
        if header[3] == 4 && header[4] & 1 == 0 {
            socket
                .write_all(&[0, 0, 0, 4, 1, 0, 0, 0, 0])
                .expect("SETTINGS ACK");
        }
        if header[3] != 1 {
            continue;
        }
        requests += 1;
        // RST_STREAM, error code 7 (REFUSED_STREAM), matching the received stream.
        let reset = [
            0, 0, 4, 3, 0, header[5], header[6], header[7], header[8], 0, 0, 0, 7,
        ];
        socket.write_all(&reset).expect("REFUSED_STREAM");
    }
}

fn refuse_connections(listener: &TcpListener, stop: &AtomicBool) -> (usize, usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut connections = Vec::new();
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((socket, _)) => {
                assert!(connections.len() < 4, "fixture exceeded connection bound");
                connections.push(thread::spawn(move || refuse_streams(socket)));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("accept: {error}"),
        }
    }
    let accepted_connections = connections.len();
    let requests = connections
        .into_iter()
        .map(|connection| connection.join().expect("connection worker"))
        .sum();
    (requests, accepted_connections)
}

async fn observed_requests(builder: reqwest::ClientBuilder, path: &str) -> usize {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let address = listener.local_addr().expect("listener address");
    let stop = Arc::new(AtomicBool::new(false));
    let server_stop = Arc::clone(&stop);
    let (ready_sender, ready_receiver) = std::sync::mpsc::sync_channel(1);
    let server = thread::spawn(move || {
        ready_sender.send(()).expect("fixture ready receiver");
        refuse_connections(&listener, &server_stop)
    });
    ready_receiver
        .recv_timeout(Duration::from_secs(3))
        .expect("fixture readiness");
    let client = builder
        .no_proxy()
        .http2_prior_knowledge()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("HTTP client");
    let result = client
        .post(format!("http://{address}{path}"))
        .body("fixed fixture body")
        .send()
        .await;
    let error = result.err();
    drop(client);
    stop.store(true, Ordering::Release);
    let (requests, accepted_connections) = server.join().expect("fixture worker");
    let error = error.expect("REFUSED_STREAM cannot produce a response");
    eprintln!(
        "HTTP/2 fixture path={path} accepted={accepted_connections} requests={requests} error={error:?}"
    );
    let mut cause = std::error::Error::source(&error);
    while let Some(source) = cause {
        eprintln!("HTTP/2 fixture cause: {source}");
        cause = source.source();
    }
    assert!(
        !error.is_timeout(),
        "fixture must return a protocol NACK, not time out: {error:?}"
    );
    requests
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn protocol_nacks_replay_with_reqwest_defaults_but_not_provider_policy() {
    for path in ["/v1/messages", "/v1/chat/completions"] {
        assert_eq!(
            observed_requests(reqwest::Client::builder(), path).await,
            3,
            "positive control must prove protocol retry behavior"
        );
        assert_eq!(
            observed_requests(http_client_builder(), path).await,
            1,
            "configured provider transport must never replay a NACK"
        );
    }
}
