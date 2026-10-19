//! Invalid complete-block protocols fail before any effect or replay checkpoint.
#[path = "provider_signed_replay/fixture.rs"]
mod fixture;
use fixture::{Mutation, Observed, agent, call, end};
use stakit_ai_sdk::{
    AgentCx, AgentError, AgentMiddleware, AssistantContent, Finish, Flow, Message, StopReason,
    StreamEvent, Thinking,
};
use std::sync::{Arc, atomic::Ordering};

const fn block(index: u64, content: AssistantContent) -> StreamEvent {
    StreamEvent::AssistantBlock { index, content }
}
fn signed() -> AssistantContent {
    AssistantContent::Thinking(Thinking::Visible {
        text: "".into(),
        signature: Some("sig".into()),
    })
}
async fn rejects(mut events: Vec<StreamEvent>) {
    events.push(call());
    events.push(end(StopReason::ToolUse));
    let (mut agent, observed) = agent(events, Mutation::None);
    let outcome = agent.run().await.expect("outcome");
    assert!(matches!(outcome.finish, Finish::Stopped { .. }));
    assert_eq!(observed.effects.load(Ordering::SeqCst), 0);
    assert_eq!(observed.requests.lock().expect("requests").len(), 1);
    assert!(agent.messages().iter().all(|message| !matches!(message, Message::Assistant(blocks) if blocks.iter().any(|block| matches!(block, AssistantContent::ToolUse { .. })))));
}
#[tokio::test]
async fn duplicate_complete_index_fails_closed() {
    rejects(vec![block(0, signed()), block(0, signed())]).await;
}
#[tokio::test]
async fn missing_complete_index_fails_closed() {
    rejects(vec![block(1, signed())]).await;
}
#[tokio::test]
async fn oversized_signature_is_rejected_without_trimming() {
    rejects(vec![block(
        0,
        AssistantContent::Thinking(Thinking::Visible {
            text: "".into(),
            signature: Some("s".repeat(1024 * 1024 + 1).into()),
        }),
    )])
    .await;
}
#[tokio::test]
async fn empty_block_count_is_bounded() {
    rejects(
        (0..1025)
            .map(|index| block(index, AssistantContent::Text("".into())))
            .collect(),
    )
    .await;
}
#[tokio::test]
async fn aggregate_complete_content_is_bounded() {
    rejects(
        (0..9)
            .map(|index| {
                block(
                    index,
                    AssistantContent::Text("x".repeat(1024 * 1024).into()),
                )
            })
            .collect(),
    )
    .await;
}
#[tokio::test]
async fn unsigned_legacy_system_mutation_remains_supported() {
    let (mut agent, observed) = agent(vec![call(), end(StopReason::ToolUse)], Mutation::System);
    assert!(matches!(
        agent.run().await.expect("outcome").finish,
        Finish::EndTurn
    ));
    assert_eq!(observed.requests.lock().expect("requests").len(), 2);
}

struct LoadSigned;
#[async_trait::async_trait]
impl AgentMiddleware<Arc<Observed>> for LoadSigned {
    async fn on_start(&self, cx: &mut AgentCx<'_, Arc<Observed>>) -> Result<Flow, AgentError> {
        cx.set_system("loaded original context");
        cx.messages_mut().push(Message::Assistant(vec![signed()]));
        cx.messages_mut().push(Message::user("continue"));
        Ok(Flow::Continue)
    }
}
#[tokio::test]
async fn on_start_can_load_trusted_signed_history_before_baseline() {
    let (mut agent, observed) = agent(vec![end(StopReason::EndTurn)], Mutation::None);
    agent = agent.register_middleware(LoadSigned);
    assert!(matches!(
        agent.run().await.expect("outcome").finish,
        Finish::EndTurn
    ));
    assert_eq!(observed.requests.lock().expect("requests").len(), 1);
}
struct ChangePrefix;
#[async_trait::async_trait]
impl AgentMiddleware<Arc<Observed>> for ChangePrefix {
    async fn on_step(&self, cx: &mut AgentCx<'_, Arc<Observed>>) -> Result<Flow, AgentError> {
        cx.messages_mut()[0] = Message::user("changed");
        Ok(Flow::Continue)
    }
}
#[tokio::test]
async fn preloaded_signed_prefix_is_frozen_before_first_on_step() {
    let (mut agent, observed) = agent(vec![end(StopReason::EndTurn)], Mutation::None);
    agent = agent
        .with_context(vec![
            Message::user("original"),
            Message::Assistant(vec![signed()]),
        ])
        .register_middleware(ChangePrefix);
    assert!(matches!(
        agent.run().await.expect("outcome").finish,
        Finish::Stopped { .. }
    ));
    assert!(observed.requests.lock().expect("requests").is_empty());
}

#[tokio::test]
async fn signature_marker_requires_complete_signed_content() {
    rejects(vec![
        StreamEvent::SignatureDelta("sig".into()),
        block(0, AssistantContent::Text("text".into())),
    ])
    .await;
}

async fn complete_only_partial(terminal: Option<StopReason>) {
    let mut events = vec![
        block(0, AssistantContent::Text("complete partial text".into())),
        block(
            1,
            AssistantContent::ToolUse {
                id: "call-1".into(),
                name: "effect".into(),
                input: serde_json::json!({}),
            },
        ),
        call(),
    ];
    if let Some(stop) = terminal {
        events.push(end(stop));
    }
    let (mut agent, observed) = agent(events, Mutation::None);
    let outcome = agent.run().await.expect("outcome");
    assert!(matches!(outcome.finish, Finish::Stopped { .. }));
    assert_eq!(outcome.text, "complete partial text");
    assert_eq!(observed.effects.load(Ordering::SeqCst), 0);
    assert_eq!(
        agent.messages().last(),
        Some(&Message::assistant("complete partial text"))
    );
}
#[tokio::test]
async fn refused_complete_only_response_keeps_text_without_tools() {
    complete_only_partial(Some(StopReason::Refusal)).await;
}
#[tokio::test]
async fn complete_blocks_without_end_never_authorize_tools() {
    complete_only_partial(None).await;
}
