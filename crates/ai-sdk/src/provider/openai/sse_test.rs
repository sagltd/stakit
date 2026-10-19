//! Fragmentation and resource limits are tested independently of HTTP chunking.
use super::decode;
use crate::{ProviderError, StopReason, StreamEvent};
use futures::{StreamExt, stream};

async fn events(chunks: Vec<Vec<u8>>) -> Vec<Result<StreamEvent, ProviderError>> {
    decode(stream::iter(
        chunks
            .into_iter()
            .map(|chunk| Ok::<_, String>(bytes::Bytes::from(chunk))),
    ))
    .collect()
    .await
}
fn wire() -> Vec<u8> {
    concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"héllo\"},\"finish_reason\":null}]}\r\n\r\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\r\n\r\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":0,\"completion_tokens\":0}}\r\n\r\n",
        "data: [DONE]\r\n\r\n"
    )
    .as_bytes()
    .to_vec()
}
#[tokio::test]
async fn every_byte_boundary_preserves_unicode_and_real_end() {
    let body = wire();
    for split in 0..=body.len() {
        let events = events(vec![body[..split].to_vec(), body[split..].to_vec()])
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .expect("valid stream");
        assert!(events.contains(&StreamEvent::TextDelta("héllo".into())));
        assert!(matches!(
            events.last(),
            Some(StreamEvent::End {
                stop: StopReason::EndTurn,
                ..
            })
        ));
    }
}
#[tokio::test]
async fn every_truncation_before_done_fails_without_end() {
    let body = wire();
    for end in 0..body.len() - 2 {
        let events = events(vec![body[..end].to_vec()]).await;
        assert!(events.last().is_some_and(Result::is_err));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Ok(StreamEvent::End { .. })))
        );
    }
}
#[tokio::test]
async fn multiline_json_and_heartbeat_are_supported() {
    let body=b": heartbeat\n\ndata: {\"choices\":\ndata: [{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":0,\"completion_tokens\":0}}\n\ndata: [DONE]\n\n";
    let events = events(vec![body.to_vec()]).await;
    assert!(matches!(events.last(), Some(Ok(StreamEvent::End { .. }))));
}
#[tokio::test]
async fn transport_failure_and_invalid_utf8_never_emit_end() {
    let source = stream::iter([
        Ok(bytes::Bytes::from_static(b"data: {\"choices\":[]")),
        Err("connection reset"),
    ]);
    let events = decode(source).collect::<Vec<_>>().await;
    assert!(matches!(
        events.last(),
        Some(Err(ProviderError::Transport(_)))
    ));
    let events = events_for_invalid_utf8().await;
    assert!(matches!(
        events.last(),
        Some(Err(ProviderError::Decode { .. }))
    ));
}
async fn events_for_invalid_utf8() -> Vec<Result<StreamEvent, ProviderError>> {
    events(vec![b"data: \xff\n\n".to_vec()]).await
}

#[tokio::test]
async fn usage_progress_survives_eof_without_authorizing_completion() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":4,\"prompt_tokens_details\":{\"cached_tokens\":3},\"completion_tokens_details\":{\"reasoning_tokens\":2}}}\n\n"
    );
    let events = events(vec![body.as_bytes().to_vec()]).await;
    assert!(events.iter().any(|event|matches!(event,Ok(StreamEvent::Usage{usage}) if usage.input_tokens==7 && usage.cache_read_tokens==3 && usage.output_tokens==4 && usage.reasoning_tokens==2)));
    assert!(matches!(
        events.last(),
        Some(Err(ProviderError::Transport(_)))
    ));
    assert!(!events.iter().any(|event| matches!(
        event,
        Ok(StreamEvent::End { .. } | StreamEvent::ToolCall { .. })
    )));
}

#[tokio::test]
async fn partial_usage_preserves_known_fields_and_refines_cache_classification() {
    let body = concat!(
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":4,\"completion_tokens_details\":{\"reasoning_tokens\":2}}}\n\n",
        "data: {\"choices\":[],\"usage\":{\"completion_tokens\":5,\"prompt_tokens_details\":{\"cached_tokens\":3}}}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"completion_tokens\":5}}\n\ndata: [DONE]\n\n"
    );
    let events = events(vec![body.as_bytes().to_vec()])
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .expect("valid cumulative usage");
    let usage = events
        .iter()
        .filter_map(|event| {
            if let StreamEvent::Usage { usage } = event {
                Some(*usage)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(usage.len(), 3);
    assert_eq!(usage[2], usage[1]);
    assert_eq!(usage[0].input_tokens, 10);
    assert_eq!(
        usage[1],
        crate::Usage {
            input_tokens: 7,
            output_tokens: 5,
            cache_read_tokens: 3,
            reasoning_tokens: 2,
            ..crate::Usage::zero()
        }
    );
    assert!(
        matches!(events.last(),Some(StreamEvent::End{usage:final_usage,..}) if final_usage==&usage[1])
    );
}
