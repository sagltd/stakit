//! A step cut short — stopped by the caller, or failed mid-stream — is still
//! metered: the provider has already processed (and billed) its prompt, so a
//! budget that only counted finished steps could be drained by stopping every
//! reply right after it starts.

use std::time::Duration;

use futures::StreamExt;
use futures::future::BoxFuture;
use stakit_ai_sdk::{
    Agent, AgentCx, AgentError, AgentEvent, AgentMiddleware, ChatRequest, ChatResponse,
    EventStream, Finish, Message, Provider, ProviderError, StreamEvent, Usage,
};

/// What the provider reports when it starts answering.
const START_USAGE: Usage = Usage {
    input_tokens: 1_000,
    output_tokens: 1,
    cache_create_tokens: 0,
    cache_read_tokens: 500,
    reasoning_tokens: 0,
};

/// How the scripted stream behaves after its first text delta.
#[derive(Clone, Copy)]
enum Tail {
    /// Never yields again (a reply the caller stops).
    Hang,
    /// Fails, like a connection reset mid-reply.
    Fail,
}

/// Starts a reply (usage + one text delta), then hangs or fails.
struct HalfReply(Tail);

impl Provider for HalfReply {
    #[allow(
        clippy::unnecessary_literal_bound,
        reason = "trait method must return &str"
    )]
    fn model_id(&self) -> &str {
        "half"
    }

    fn complete(&self, _r: ChatRequest) -> BoxFuture<'_, Result<ChatResponse, ProviderError>> {
        Box::pin(async { Err(ProviderError::Cancelled) })
    }

    fn stream(&self, _r: ChatRequest) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
        let tail = self.0;
        Box::pin(async move {
            let head = futures::stream::iter(vec![
                Ok(StreamEvent::Start { usage: START_USAGE }),
                Ok(StreamEvent::TextDelta("partial answer".into())),
            ]);
            Ok(match tail {
                Tail::Hang => head.chain(futures::stream::pending()).boxed(),
                Tail::Fail => head
                    .chain(futures::stream::iter(vec![Err(ProviderError::Transport(
                        "connection reset".into(),
                    ))]))
                    .boxed(),
            })
        })
    }
}

/// Accepts the request but never answers (a stop lands while it is in flight).
struct Silent;

impl Provider for Silent {
    #[allow(
        clippy::unnecessary_literal_bound,
        reason = "trait method must return &str"
    )]
    fn model_id(&self) -> &str {
        "silent"
    }

    fn complete(&self, _r: ChatRequest) -> BoxFuture<'_, Result<ChatResponse, ProviderError>> {
        Box::pin(async { Err(ProviderError::Cancelled) })
    }

    fn stream(&self, _r: ChatRequest) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
        Box::pin(futures::future::pending())
    }
}

/// Records what `on_finish` saw, like a budget middleware would.
struct SeenAtFinish(std::sync::Arc<std::sync::Mutex<Option<Usage>>>);

#[async_trait::async_trait]
impl AgentMiddleware<()> for SeenAtFinish {
    async fn on_finish(&self, cx: &AgentCx<'_, ()>) -> Result<(), AgentError> {
        *self.0.lock().expect("lock") = Some(*cx.usage());
        Ok(())
    }
}

/// Run `agent` and cancel it once its first text delta arrives.
async fn stop_after_first_delta(agent: &mut Agent<()>) -> stakit_ai_sdk::Outcome {
    let cancel = agent.cancel_token();
    let mut run = agent.run();
    while let Some(event) = run.next().await {
        match event {
            AgentEvent::MessageDelta(_) => cancel.cancel(),
            AgentEvent::Done(outcome) => return outcome,
            _ => {}
        }
    }
    panic!("the run ended without an outcome");
}

#[tokio::test]
async fn a_stopped_step_still_counts_its_prompt_and_streamed_text() {
    let seen = std::sync::Arc::default();
    let mut agent = Agent::new(())
        .provider(HalfReply(Tail::Hang))
        .register_middleware(SeenAtFinish(std::sync::Arc::clone(&seen)))
        .with_context(vec![Message::user("hi")]);

    let outcome = stop_after_first_delta(&mut agent).await;

    assert!(matches!(outcome.finish, Finish::Cancelled));
    assert_eq!(outcome.usage.input_tokens, 1_000);
    assert_eq!(outcome.usage.cache_read_tokens, 500);
    // "partial answer" is 14 bytes: about 4 tokens, more than the 1 reported.
    assert_eq!(outcome.usage.output_tokens, 4);
    let at_finish = seen.lock().expect("lock").expect("on_finish ran");
    assert_eq!(at_finish, outcome.usage, "on_finish meters the cut step");
}

#[tokio::test]
async fn a_step_that_fails_mid_stream_still_counts_its_prompt() {
    let mut agent = Agent::new(())
        .provider(HalfReply(Tail::Fail))
        .with_context(vec![Message::user("hi")]);

    let outcome = agent.run().await.expect("outcome");

    assert!(matches!(outcome.finish, Finish::Stopped { .. }));
    assert_eq!(outcome.usage.input_tokens, 1_000);
    assert_eq!(outcome.usage.cache_read_tokens, 500);
}

#[tokio::test]
async fn a_stop_while_the_request_is_in_flight_counts_the_estimated_prompt() {
    let prompt = "x".repeat(4_000);
    let mut agent = Agent::new(())
        .provider(Silent)
        .with_context(vec![Message::user(prompt)]);
    let cancel = agent.cancel_token();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
    });

    let outcome = agent.run().await.expect("outcome");

    assert!(matches!(outcome.finish, Finish::Cancelled));
    // 4 000 bytes of prompt at ~4 bytes per token.
    assert!(
        outcome.usage.input_tokens >= 1_000,
        "estimated prompt tokens: {}",
        outcome.usage.input_tokens
    );
}

#[tokio::test]
async fn a_stop_before_any_request_counts_nothing() {
    let mut agent = Agent::new(())
        .provider(Silent)
        .with_context(vec![Message::user("hi")]);
    agent.cancel_token().cancel();

    let outcome = agent.run().await.expect("outcome");

    assert!(matches!(outcome.finish, Finish::Cancelled));
    assert_eq!(outcome.usage, Usage::default());
}
