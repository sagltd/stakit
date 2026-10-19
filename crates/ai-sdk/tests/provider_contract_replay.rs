//! Provider conformance includes the exact replay rules enforced by Agent.
#[path = "provider_contract/scripted.rs"]
#[allow(
    dead_code,
    reason = "replay probes share provider fixtures with completion and fitting probes"
)]
mod scripted;
use scripted::{ScriptedProvider, request, usage};
use stakit_ai_sdk::{
    AssistantContent, StopReason, StreamEvent, Thinking, test_support::contract::assert_stream,
};
fn call() -> StreamEvent {
    StreamEvent::ToolCall {
        id: "call".into(),
        name: "effect".into(),
        arguments: "{}".into(),
    }
}
fn block(index: u64) -> StreamEvent {
    StreamEvent::AssistantBlock {
        index,
        content: AssistantContent::ToolUse {
            id: "call".into(),
            name: "effect".into(),
            input: serde_json::json!({}),
        },
    }
}
const fn end() -> StreamEvent {
    StreamEvent::End {
        stop: StopReason::ToolUse,
        usage: usage(),
    }
}

#[tokio::test]
async fn signed_or_complete_replay_cannot_bypass_agent_validation() {
    for events in [
        vec![StreamEvent::SignatureDelta("sig".into()), call(), end()],
        vec![block(0), end()],
        vec![call(), block(0), block(1), end()],
        vec![
            call(),
            StreamEvent::AssistantBlock {
                index: 0,
                content: AssistantContent::ToolUse {
                    id: "different".into(),
                    name: "effect".into(),
                    input: serde_json::json!({}),
                },
            },
            end(),
        ],
        vec![
            call(),
            StreamEvent::AssistantBlock {
                index: 0,
                content: AssistantContent::ToolUse {
                    id: "call".into(),
                    name: "effect".into(),
                    input: serde_json::json!({"fabricated":true}),
                },
            },
            end(),
        ],
    ] {
        let provider = ScriptedProvider {
            events,
            failure: None,
        };
        assert!(
            assert_stream(&provider, request(), &StopReason::ToolUse, &usage())
                .await
                .is_err()
        );
    }
    let provider = ScriptedProvider {
        events: vec![
            StreamEvent::SignatureDelta("sig".into()),
            StreamEvent::AssistantBlock {
                index: 0,
                content: AssistantContent::Thinking(Thinking::Visible {
                    text: "reasoning".into(),
                    signature: Some("sig".into()),
                }),
            },
            call(),
            block(1),
            end(),
        ],
        failure: None,
    };
    assert_stream(&provider, request(), &StopReason::ToolUse, &usage())
        .await
        .expect("complete signed replay passes");
}
