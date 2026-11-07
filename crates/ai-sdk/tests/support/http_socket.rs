//! Bounded socket setup shared by provider HTTP fixtures.

use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

/// Accepts one fixture connection with a bounded wait and socket I/O deadlines.
pub(crate) fn accept(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(5);
    let socket = loop {
        match listener.accept() {
            Ok((socket, _)) => break socket,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("fixture accept: {error}"),
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
    socket
}
