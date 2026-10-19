use super::Decoder;
use super::sse_test::{collect, frame, text_and_tool};
use crate::provider::sse::MAX_FRAME_BYTES as MAX_SSE_LINE;
use crate::{ProviderError, StreamEvent};
use bytes::Bytes;

#[test]
fn one_byte_chunks_are_buffered_once_up_to_the_line_limit() {
    let mut decoder = Decoder::default();
    for _ in 0..MAX_SSE_LINE {
        assert!(
            decoder
                .accept(b"x")
                .expect("bounded partial line")
                .is_empty()
        );
    }
    assert!(decoder.accept(b"x").is_err());
    // The rejected fragment is not appended: the original line still fits.
    assert!(decoder.accept(b"\n").expect("line at exact cap").is_empty());
}

#[test]
fn multiple_data_lines_share_one_bounded_event_buffer() {
    let mut decoder = Decoder::default();
    let line = format!("data: {}\n", "x".repeat(1024));
    let mut rejected = false;
    for _ in 0..=MAX_SSE_LINE / 1024 {
        if let Err(error) = decoder.accept(line.as_bytes()) {
            assert!(matches!(error, ProviderError::Decode { .. }));
            rejected = true;
            break;
        }
    }
    assert!(
        rejected,
        "small individually valid lines cannot grow an event forever"
    );
}

#[tokio::test]
async fn newline_does_not_bypass_the_line_size_cap() {
    let mut bytes = vec![b'x'; MAX_SSE_LINE + 1];
    bytes.push(b'\n');
    let events = collect(vec![Bytes::from(bytes)]).await;
    assert!(matches!(
        events.last(),
        Some(Err(ProviderError::Decode { .. }))
    ));
}

#[tokio::test]
async fn many_small_frames_in_one_large_chunk_complete_without_a_whole_chunk_queue() {
    let mut wire = "data: {\"type\":\"ping\"}\n\n".repeat(50_000);
    assert!(wire.len() > MAX_SSE_LINE);
    wire.extend(text_and_tool().iter().map(frame));
    let events = collect(vec![Bytes::from(wire)]).await;
    assert!(events.iter().all(Result::is_ok));
    assert!(matches!(events.last(), Some(Ok(StreamEvent::End { .. }))));
}
