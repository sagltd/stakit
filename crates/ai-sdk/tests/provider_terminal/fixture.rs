//! An incomplete or unsafe provider turn must never authorize a tool effect.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use stakit_ai_sdk::{
    Agent, AgentCx, AgentError, AgentMiddleware, Approval, AssistantContent, Finish, Message,
    PendingToolCall, ProviderError, StopReason, StreamEvent, Usage, event_stream, tool,
};

use super::mock::{Scripted, stream_of, tool_call};

#[derive(Clone)]
pub(super) enum Terminal {
    End(StopReason),
    EndWithoutTools(StopReason),
    Eof,
    Error,
    Empty,
}

#[derive(Default)]
pub(super) struct Observed {
    pub(super) requests: AtomicUsize,
    pub(super) effects: AtomicUsize,
    pub(super) approvals: AtomicUsize,
    pub(super) finishes: AtomicUsize,
}

#[derive(serde::Deserialize, stakit_model::Model, stakit_model::JsonSchema)]
struct EffectArgs {}

#[tool(name = "effect", description = "Perform the recorded effect.")]
fn effect(
    cx: &stakit_ai_sdk::ToolCx<Arc<Observed>>,
    _args: EffectArgs,
) -> Result<&'static str, ToolError> {
    cx.ctx().effects.fetch_add(1, Ordering::SeqCst);
    Ok("done")
}

/// Streams `terminal` on the first request and a plain final answer after.
fn recording(observed: &Arc<Observed>, terminal: Terminal) -> Scripted {
    let observed = Arc::clone(observed);
    Scripted::new("recording", move |_, _| {
        let first = observed.requests.fetch_add(1, Ordering::SeqCst) == 0;
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 4,
            ..Usage::zero()
        };
        if !first {
            return Ok(stream_of(vec![
                StreamEvent::TextDelta("finished".into()),
                StreamEvent::End {
                    stop: StopReason::EndTurn,
                    usage,
                },
            ]));
        }
        if matches!(terminal, Terminal::Empty) {
            return Ok(stream_of(Vec::new()));
        }
        let mut events = vec![
            Ok(StreamEvent::Start { usage }),
            Ok(StreamEvent::TextDelta("partial answer".into())),
        ];
        if !matches!(terminal, Terminal::EndWithoutTools(_)) {
            events.push(Ok(tool_call("effect-1", "effect", "{}")));
        }
        match terminal.clone() {
            Terminal::End(stop) | Terminal::EndWithoutTools(stop) => {
                events.push(Ok(StreamEvent::End { stop, usage }));
            }
            Terminal::Error => events.push(Err(ProviderError::Transport("interrupted".into()))),
            Terminal::Eof | Terminal::Empty => {}
        }
        Ok(event_stream(events))
    })
}

struct ObserveFinish(Arc<Observed>);

#[async_trait::async_trait]
impl AgentMiddleware<Arc<Observed>> for ObserveFinish {
    async fn on_tool_approve(
        &self,
        _cx: &AgentCx<'_, Arc<Observed>>,
        _call: &PendingToolCall,
    ) -> Result<Approval, AgentError> {
        self.0.approvals.fetch_add(1, Ordering::SeqCst);
        Ok(Approval::Allow)
    }

    async fn on_finish(&self, _cx: &AgentCx<'_, Arc<Observed>>) -> Result<(), AgentError> {
        self.0.finishes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

pub(super) fn scripted(terminal: Terminal) -> (Agent<Arc<Observed>>, Arc<Observed>) {
    let observed = Arc::new(Observed::default());
    let agent = Agent::new(Arc::clone(&observed))
        .provider(recording(&observed, terminal))
        .register_tool(effect)
        .register_middleware(ObserveFinish(Arc::clone(&observed)))
        .with_context(vec![Message::user("go")]);
    (agent, observed)
}

pub(super) async fn assert_terminal_is_safe(terminal: Terminal, has_partial: bool) {
    let (mut agent, observed) = scripted(terminal);
    let outcome = agent.run().await.expect("outcome");
    assert_eq!(
        observed.effects.load(Ordering::SeqCst),
        0,
        "unsafe effect ran"
    );
    assert_eq!(
        observed.approvals.load(Ordering::SeqCst),
        0,
        "unsafe call reached approvals"
    );
    assert_eq!(
        observed.requests.load(Ordering::SeqCst),
        1,
        "unsafe turn retried"
    );
    assert_eq!(observed.finishes.load(Ordering::SeqCst), 1);
    assert!(matches!(outcome.finish, Finish::Stopped { .. }));
    if has_partial {
        assert_eq!(outcome.text, "partial answer");
        assert_eq!(outcome.usage.input_tokens, 100);
        assert!(outcome.usage.output_tokens >= 4);
        assert!(
            matches!(agent.messages().last(), Some(Message::Assistant(blocks))
            if matches!(blocks.as_slice(), [AssistantContent::Text(text)] if &**text == "partial answer"))
        );
    }
    assert!(
        agent.messages().iter().all(|message| match message {
            Message::Assistant(blocks) => blocks
                .iter()
                .all(|block| !matches!(block, AssistantContent::ToolUse { .. })),
            Message::User(_) => true,
        }),
        "unsafe tool call persisted into resumable history"
    );
}
