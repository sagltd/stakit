//! Reads one bounded HTTP/1.1 request from a loopback fixture socket. Shared by
//! the provider unit tests and the adapter contract probes.

use std::io::Read;

/// Reads headers (at most 64 KiB) and a `Content-Length` body of at most
/// `max_body` bytes. Returns the request bytes and where the body starts.
pub(crate) fn read_request(socket: &mut impl Read, max_body: usize) -> (Vec<u8>, usize) {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    let header_end = loop {
        assert!(
            bytes.len() <= 64 * 1024,
            "fixture request headers too large"
        );
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break index + 4;
        }
        let count = socket.read(&mut buffer).expect("request headers");
        assert!(count > 0, "missing request headers");
        bytes.extend_from_slice(&buffer[..count]);
    };
    let header = std::str::from_utf8(&bytes[..header_end]).expect("UTF-8 request headers");
    let length = header
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().expect("body length"))
        })
        .expect("content length");
    assert!(length <= max_body, "fixture request body too large");
    while bytes.len() - header_end < length {
        let count = socket.read(&mut buffer).expect("request body");
        assert!(count > 0, "missing request body");
        bytes.extend_from_slice(&buffer[..count]);
    }
    bytes.truncate(header_end + length);
    (bytes, header_end)
}
