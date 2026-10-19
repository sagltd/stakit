#![allow(
    dead_code,
    reason = "Shared fixture is included by suites using different subsets"
)]
use stakit_ai_sdk::{
    Agent, AgentCx, AgentError, AgentMiddleware, AssistantContent, ChatRequest, Finish, Flow,
    Message, Outcome, StopReason, StreamEvent, Thinking, Usage,
};

use super::mock::{CountsEffects, EffectTool, Scripted, stream_of};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Default)]
pub(super) struct Observed {
    pub(super) requests: Mutex<Vec<ChatRequest>>,
    pub(super) effects: AtomicUsize,
}

impl Observed {
    /// How many requests the providers received.
    pub(super) fn request_count(&self) -> usize {
        self.requests.lock().expect("requests").len()
    }

    /// How many times the effect tool ran.
    pub(super) fn effect_count(&self) -> usize {
        self.effects.load(Ordering::SeqCst)
    }
}

#[derive(Clone, Copy)]
pub(super) enum Mutation {
    None,
    System,
    Prefix,
    Model,
}

pub(super) struct Mutate(pub(super) Mutation);
#[async_trait::async_trait]
impl AgentMiddleware<Arc<Observed>> for Mutate {
    async fn on_step(&self, cx: &mut AgentCx<'_, Arc<Observed>>) -> Result<Flow, AgentError> {
        if cx.index() == 1 {
            match self.0 {
                Mutation::None => {}
                Mutation::System => cx.set_system("changed system"),
                Mutation::Prefix => cx.messages_mut()[0] = Message::user("changed prefix"),
                Mutation::Model => cx.set_model("replacement"),
            }
        }
        Ok(Flow::Continue)
    }
}

impl CountsEffects for Observed {
    fn effects(&self) -> &AtomicUsize {
        &self.effects
    }
}

/// Serves `model`, logging every request in `observed`; the run's first
/// request (across providers) gets `first`, later ones a final answer.
fn recording(observed: &Arc<Observed>, first: Vec<StreamEvent>, model: &'static str) -> Scripted {
    let observed = Arc::clone(observed);
    Scripted::new(model, move |_, request| {
        let mut requests = observed.requests.lock().expect("requests");
        let is_first = requests.is_empty();
        requests.push(request.clone());
        drop(requests);
        Ok(stream_of(if is_first {
            first.clone()
        } else {
            vec![
                StreamEvent::TextDelta("done".into()),
                end(StopReason::EndTurn),
            ]
        }))
    })
}

pub(super) const fn end(stop: StopReason) -> StreamEvent {
    StreamEvent::End {
        stop,
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Usage::zero()
        },
    }
}
pub(super) fn call() -> StreamEvent {
    StreamEvent::ToolCall {
        id: "call-1".into(),
        name: "effect".into(),
        arguments: "{}".into(),
    }
}
pub(super) fn agent(
    events: Vec<StreamEvent>,
    mutation: Mutation,
) -> (Agent<Arc<Observed>>, Arc<Observed>) {
    let observed = Arc::new(Observed::default());
    let agent = Agent::new(Arc::clone(&observed))
        .provider(recording(&observed, events, "recording"))
        .register_provider(recording(&observed, vec![], "replacement"))
        .system("original system")
        .register_tool(EffectTool)
        .register_middleware(Mutate(mutation))
        .with_context(vec![Message::user("original prefix")]);
    (agent, observed)
}

/// An empty visible thinking block carrying `signature`.
pub(super) fn signed(signature: &str) -> AssistantContent {
    AssistantContent::Thinking(Thinking::Visible {
        text: "".into(),
        signature: Some(signature.into()),
    })
}

/// Runs `agent`, asserting it ends its turn.
pub(super) async fn assert_ends_turn(agent: &mut Agent<Arc<Observed>>) {
    let finish = agent.run().await.expect("outcome").finish;
    assert!(matches!(finish, Finish::EndTurn), "{finish:?}");
}

/// Runs `agent`, asserting it stops before the effect tool ever runs.
pub(super) async fn assert_stops_without_effects(
    agent: &mut Agent<Arc<Observed>>,
    observed: &Observed,
) -> Outcome {
    let outcome = agent.run().await.expect("outcome");
    assert!(
        matches!(outcome.finish, Finish::Stopped { .. }),
        "{:?}",
        outcome.finish
    );
    assert_eq!(observed.effect_count(), 0);
    outcome
}
