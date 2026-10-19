//! Signed replay uses complete ordered blocks and an immutable request prefix.
#[path = "provider_signed_replay/fixture.rs"]
mod fixture;
#[path = "support/mock.rs"]
mod mock;
use fixture::{Mutation, agent, assert_ends_turn, assert_stops_without_effects, call, end, signed};
use serde_json::json;
use stakit_ai_sdk::{AssistantContent, Finish, Message, StopReason, StreamEvent, Thinking};

fn tool_block() -> AssistantContent {
    AssistantContent::ToolUse {
        id: "call-1".into(),
        name: "effect".into(),
        input: json!({}),
    }
}
fn replay_blocks() -> Vec<AssistantContent> {
    vec![
        signed("sig-one"),
        tool_block(),
        AssistantContent::Text("between".into()),
        signed("sig-two"),
        AssistantContent::Thinking(Thinking::Redacted {
            data: "opaque-data".into(),
        }),
    ]
}
fn signed_events() -> Vec<StreamEvent> {
    let mut events = vec![
        StreamEvent::TextDelta("between".into()),
        StreamEvent::SignatureDelta("sig-one".into()),
        StreamEvent::SignatureDelta("sig-two".into()),
        call(),
    ];
    // Block completion order need not equal original response order.
    for index in [0, 3, 4, 2, 1] {
        events.push(StreamEvent::AssistantBlock {
            index,
            content: replay_blocks()[usize::try_from(index).expect("index")].clone(),
        });
    }
    events.push(end(StopReason::ToolUse));
    events
}

#[tokio::test]
async fn preserves_separate_empty_signed_redacted_blocks_and_original_order() {
    let (mut agent, observed) = agent(signed_events(), Mutation::None);
    let outcome = agent.run().await.expect("outcome");
    assert!(matches!(outcome.finish, Finish::EndTurn));
    assert_eq!(observed.effect_count(), 1);
    let requests = observed.requests.lock().expect("requests");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].messages[1], Message::Assistant(replay_blocks()));
    assert!(matches!(requests[1].messages[2], Message::User(_)));
    drop(requests);
}

async fn assert_mutation_stops(mutation: Mutation) {
    let (mut agent, observed) = agent(signed_events(), mutation);
    let outcome = agent.run().await.expect("outcome");
    assert!(matches!(outcome.finish, Finish::Stopped { .. }));
    assert_eq!(observed.request_count(), 1, "invalid signed context sent");
    assert_eq!(observed.effect_count(), 1);
    assert_eq!(outcome.usage.input_tokens, 10);
}
#[tokio::test]
async fn signed_system_mutation_stops_before_next_request() {
    assert_mutation_stops(Mutation::System).await;
}
#[tokio::test]
async fn signed_prefix_mutation_stops_before_next_request() {
    assert_mutation_stops(Mutation::Prefix).await;
}
#[tokio::test]
async fn signed_model_mutation_stops_before_next_request() {
    assert_mutation_stops(Mutation::Model).await;
}

#[tokio::test]
async fn signature_deltas_without_complete_blocks_never_dispatch() {
    let events = vec![
        StreamEvent::ReasoningDelta("reason".into()),
        StreamEvent::SignatureDelta("signature".into()),
        call(),
        end(StopReason::ToolUse),
    ];
    let (mut agent, observed) = agent(events, Mutation::None);
    assert_stops_without_effects(&mut agent, &observed).await;
    assert_eq!(observed.request_count(), 1);
    assert_eq!(agent.messages().len(), 1);
}

#[tokio::test]
async fn incomplete_complete_tool_protocol_never_dispatches() {
    let events = vec![
        StreamEvent::AssistantBlock {
            index: 0,
            content: signed("signature"),
        },
        call(),
        end(StopReason::ToolUse),
    ];
    let (mut agent, observed) = agent(events, Mutation::None);
    assert_stops_without_effects(&mut agent, &observed).await;
}

#[tokio::test]
async fn preloaded_signed_history_is_trusted_with_original_context() {
    let (mut agent, observed) = agent(vec![end(StopReason::EndTurn)], Mutation::None);
    agent = agent.with_context(vec![
        Message::user("original prefix"),
        Message::Assistant(vec![signed("preloaded")]),
        Message::user("continue"),
    ]);
    assert_ends_turn(&mut agent).await;
    assert_eq!(observed.request_count(), 1);
}

#[tokio::test]
async fn signed_context_remains_bound_across_runs() {
    let (mut agent, observed) = agent(signed_events(), Mutation::None);
    assert!(matches!(
        agent.run().await.expect("first outcome").finish,
        Finish::EndTurn
    ));
    assert_eq!(observed.request_count(), 2);
    agent = agent.system("changed between runs");
    assert!(matches!(
        agent.run().await.expect("second outcome").finish,
        Finish::Stopped { .. }
    ));
    assert_eq!(observed.request_count(), 2);
}

#[tokio::test]
async fn signed_context_allows_new_appended_turns() {
    let (mut agent, observed) = agent(signed_events(), Mutation::None);
    let _ = agent.run().await.expect("first outcome");
    let mut messages = agent.messages().to_vec();
    messages.push(Message::user("a later question"));
    agent = agent.with_context(messages);
    assert!(matches!(
        agent.run().await.expect("second outcome").finish,
        Finish::EndTurn
    ));
    assert_eq!(observed.request_count(), 3);
}
