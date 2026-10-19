use futures::future::BoxFuture;
use stakit_ai_sdk::{
    Agent, AgentCx, AgentError, AgentMiddleware, ChatRequest, ChatResponse, EventStream, Flow,
    Message, Provider, ProviderError, StopReason, StreamEvent, Usage, event_stream, tool,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Default)]
pub(super) struct Observed {
    pub(super) requests: Mutex<Vec<ChatRequest>>,
    pub(super) effects: AtomicUsize,
}

#[derive(Clone, Copy)]
#[allow(
    dead_code,
    reason = "Shared fixture is included by suites using different mutation subsets"
)]
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

#[derive(serde::Deserialize, stakit_model::Model, stakit_model::JsonSchema)]
struct EffectArgs {}
#[tool(name = "effect", description = "Record an effect.")]
fn effect(
    cx: &stakit_ai_sdk::ToolCx<Arc<Observed>>,
    _args: EffectArgs,
) -> Result<&'static str, ToolError> {
    cx.ctx().effects.fetch_add(1, Ordering::SeqCst);
    Ok("done")
}

struct RecordingProvider {
    observed: Arc<Observed>,
    first: Vec<StreamEvent>,
    model: &'static str,
}
impl Provider for RecordingProvider {
    fn model_id(&self) -> &str {
        self.model
    }
    fn complete(
        &self,
        _request: ChatRequest,
    ) -> BoxFuture<'_, Result<ChatResponse, ProviderError>> {
        Box::pin(async { Err(ProviderError::Cancelled) })
    }
    fn stream(&self, request: ChatRequest) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
        let mut requests = self.observed.requests.lock().expect("requests");
        let first = requests.is_empty();
        requests.push(request);
        drop(requests);
        let events = if first {
            self.first.clone()
        } else {
            vec![
                StreamEvent::TextDelta("done".into()),
                end(StopReason::EndTurn),
            ]
        };
        Box::pin(async move { Ok(event_stream(events.into_iter().map(Ok).collect())) })
    }
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
        .provider(RecordingProvider {
            observed: Arc::clone(&observed),
            first: events,
            model: "recording",
        })
        .register_provider(RecordingProvider {
            observed: Arc::clone(&observed),
            first: vec![],
            model: "replacement",
        })
        .system("original system")
        .register_tool(effect)
        .register_middleware(Mutate(mutation))
        .with_context(vec![Message::user("original prefix")]);
    (agent, observed)
}
