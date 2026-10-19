//! Only a complete tool-use turn authorizes effects, never refusal or truncation.
#[path = "provider_terminal/fixture.rs"]
mod fixture;
#[path = "support/mock.rs"]
mod mock;

use fixture::{Terminal, assert_terminal_is_safe, scripted};
use futures::StreamExt as _;
use stakit_ai_sdk::{AgentEvent, Finish, StopReason};
use std::sync::atomic::Ordering;

#[tokio::test]
async fn refusal_never_dispatches_or_retries_queued_tools() {
    assert_terminal_is_safe(Terminal::End(StopReason::Refusal), true).await;
}

#[tokio::test]
async fn output_limit_never_dispatches_queued_tools() {
    assert_terminal_is_safe(Terminal::End(StopReason::MaxTokens), true).await;
}

#[tokio::test]
async fn paused_turn_never_dispatches_queued_tools() {
    assert_terminal_is_safe(Terminal::End(StopReason::Pause), true).await;
}

#[tokio::test]
async fn unknown_terminal_reason_never_dispatches_queued_tools() {
    assert_terminal_is_safe(Terminal::End(StopReason::Other("future".into())), true).await;
}

#[tokio::test]
async fn eof_never_dispatches_queued_tools_or_claims_completion() {
    assert_terminal_is_safe(Terminal::Eof, true).await;
}

#[tokio::test]
async fn transport_failure_keeps_partial_text_without_queued_tools() {
    assert_terminal_is_safe(Terminal::Error, true).await;
}

#[tokio::test]
async fn empty_stream_is_not_a_successful_turn() {
    assert_terminal_is_safe(Terminal::Empty, false).await;
}

#[tokio::test]
async fn inconsistent_end_turn_never_dispatches_queued_tools() {
    assert_terminal_is_safe(Terminal::End(StopReason::EndTurn), true).await;
}

#[tokio::test]
async fn stop_sequence_never_dispatches_queued_tools() {
    assert_terminal_is_safe(Terminal::End(StopReason::StopSequence("done".into())), true).await;
}

#[tokio::test]
async fn valid_tool_use_executes_once_and_continues() {
    let (mut agent, observed) = scripted(Terminal::End(StopReason::ToolUse));
    let outcome = agent.run().await.expect("outcome");
    assert_eq!(
        observed.effects.load(Ordering::SeqCst),
        1,
        "{outcome:?} {:?}",
        agent.messages()
    );
    assert_eq!(observed.approvals.load(Ordering::SeqCst), 1);
    assert_eq!(observed.requests.load(Ordering::SeqCst), 2);
    assert_eq!(observed.finishes.load(Ordering::SeqCst), 1);
    assert!(matches!(outcome.finish, Finish::EndTurn));
    assert_eq!(outcome.text, "finished");
}

#[tokio::test]
async fn refusal_without_tools_is_still_not_successful_completion() {
    assert_terminal_is_safe(Terminal::EndWithoutTools(StopReason::Refusal), true).await;
}

#[tokio::test]
async fn tool_use_without_calls_is_an_incomplete_protocol_turn() {
    assert_terminal_is_safe(Terminal::EndWithoutTools(StopReason::ToolUse), true).await;
}

#[tokio::test]
async fn valid_stop_sequence_without_tools_completes_normally() {
    let (mut agent, observed) = scripted(Terminal::EndWithoutTools(StopReason::StopSequence(
        "done".into(),
    )));
    let outcome = agent.run().await.expect("outcome");
    assert!(matches!(outcome.finish, Finish::EndTurn));
    assert_eq!(outcome.text, "partial answer");
    assert_eq!(observed.effects.load(Ordering::SeqCst), 0);
    assert_eq!(observed.requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancellation_keeps_partial_text_and_finishes_once_without_dispatch() {
    let (mut agent, observed) = scripted(Terminal::Eof);
    let cancel = agent.cancel_token();
    let mut run = agent.run();
    let outcome = loop {
        match run.next().await.expect("run event") {
            AgentEvent::ToolCall { .. } => cancel.cancel(),
            AgentEvent::Done(outcome) => break outcome,
            _ => {}
        }
    };
    drop(run);
    assert!(matches!(outcome.finish, Finish::Cancelled));
    assert_eq!(outcome.text, "partial answer");
    assert_eq!(outcome.usage.input_tokens, 100);
    assert_eq!(observed.effects.load(Ordering::SeqCst), 0);
    assert_eq!(observed.approvals.load(Ordering::SeqCst), 0);
    assert_eq!(observed.finishes.load(Ordering::SeqCst), 1);
}
