//! Fixtures shared by the built-in providers' stream and usage tests.

use std::fmt::Write as _;

use futures::StreamExt as _;
use serde_json::Value;

use crate::provider::sse::ByteStream;
use crate::{EventStream, ProviderError, StreamEvent, Usage};

/// Everything a decoded stream yielded, in order.
pub(crate) type Events = Vec<Result<StreamEvent, ProviderError>>;

/// One SSE body with a `data:` frame per event.
pub(crate) fn sse_wire<'a>(events: impl IntoIterator<Item = &'a Value>) -> String {
    let mut wire = String::new();
    for event in events {
        write!(wire, "data: {event}\n\n").expect("write fixture");
    }
    wire
}

/// A response body delivering `wire` as a single chunk.
pub(crate) fn one_chunk(wire: String) -> impl ByteStream {
    futures::stream::iter([Ok::<_, String>(bytes::Bytes::from(wire))])
}

/// Drains a decoded stream.
pub(crate) async fn collect(stream: EventStream) -> Events {
    stream.collect().await
}

/// The usage of the terminal [`StreamEvent::End`], if the stream certified one.
pub(crate) fn end_usage(events: &Events) -> Option<Usage> {
    match events.last() {
        Some(Ok(StreamEvent::End { usage, .. })) => Some(*usage),
        _ => None,
    }
}

/// Asserts the stream failed with a payload-free decode error and never
/// certified completion: no [`StreamEvent::End`] and no released tool call.
pub(crate) fn assert_uncertified(events: &Events) {
    assert!(
        matches!(events.last(), Some(Err(ProviderError::Decode { body, .. })) if body.is_empty()),
        "{events:?}"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            Ok(StreamEvent::End { .. } | StreamEvent::ToolCall { .. })
        )),
        "{events:?}"
    );
}

/// Asserts `result` failed with [`ProviderError::Decode`].
pub(crate) fn expect_decode_error<T: std::fmt::Debug>(result: Result<T, ProviderError>) {
    let err = result.expect_err("expected a decode error");
    assert!(matches!(err, ProviderError::Decode { .. }), "{err:?}");
}

/// Asserts a stream decoder accepts [`crate::provider::MAX_STREAM_OPEN_BLOCKS`]
/// distinct tool calls (`open(index)` opens one) and rejects one more.
pub(crate) fn assert_open_tools_capped<T: std::fmt::Debug>(
    mut open: impl FnMut(u64) -> Result<T, ProviderError>,
) {
    let cap = crate::provider::MAX_STREAM_OPEN_BLOCKS as u64;
    for index in 0..cap {
        open(index).expect("under cap");
    }
    expect_decode_error(open(cap));
}
