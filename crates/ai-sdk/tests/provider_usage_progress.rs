//! Progress usage survives incomplete streams without authorizing tool effects.
use futures::{StreamExt, future::BoxFuture};
use stakit_ai_sdk::{
    Agent, CancelToken, ChatRequest, ChatResponse, EventStream, Finish, ModelPrice, Pricing,
    Provider, ProviderError, StreamEvent, Usage, event_stream, tool,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Clone, Copy)]
enum Ending {
    Error,
    Eof,
    Cancel,
}
struct ProgressProvider {
    ending: Ending,
    cancel: CancelToken,
}
const fn usage(hourly: u64, output: u64) -> Usage {
    Usage {
        input_tokens: 90,
        cache_create_tokens: 10,
        cache_create_1h_tokens: hourly,
        output_tokens: output,
        ..Usage::zero()
    }
}
impl Provider for ProgressProvider {
    #[expect(
        clippy::unnecessary_literal_bound,
        reason = "Provider borrows model id"
    )]
    fn model_id(&self) -> &str {
        "progress-model"
    }
    fn complete(
        &self,
        _request: ChatRequest,
    ) -> BoxFuture<'_, Result<ChatResponse, ProviderError>> {
        Box::pin(async { Err(ProviderError::Cancelled) })
    }
    fn stream(&self, _request: ChatRequest) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
        let ending = self.ending;
        let cancel = self.cancel.clone();
        Box::pin(async move {
            let mut events = event_stream(vec![
                Ok(StreamEvent::Start { usage: usage(4, 1) }),
                Ok(StreamEvent::TextDelta("kept".into())),
                Ok(StreamEvent::Usage { usage: usage(6, 5) }),
                Ok(StreamEvent::Usage { usage: usage(7, 8) }),
                Ok(StreamEvent::ToolCall {
                    id: "effect-1".into(),
                    name: "effect".into(),
                    arguments: "{}".into(),
                }),
            ]);
            let stream = async_stream::stream! {
                while let Some(event) = events.next().await { yield event; }
                match ending {
                    Ending::Error => yield Err(ProviderError::Transport("interrupted".into())),
                    Ending::Eof => {},
                    Ending::Cancel => { cancel.cancel(); std::future::pending::<()>().await; },
                }
            };
            Ok(stream.boxed())
        })
    }
}
#[tool(description = "Record a side effect.")]
fn effect(cx: &stakit_ai_sdk::ToolCx<Arc<AtomicUsize>>) -> Result<(), ToolError> {
    cx.ctx().fetch_add(1, Ordering::SeqCst);
    Ok(())
}
const BASE: ModelPrice = ModelPrice {
    input: 1.0,
    output: 10.0,
    cache_write: 2.5,
    cache_write_1h: Some(4.0),
    cache_read: 0.1,
};
async fn assert_partial_progress(ending: Ending) {
    let effects = Arc::new(AtomicUsize::new(0));
    let mut agent = Agent::new(Arc::clone(&effects)).register_tool(effect);
    let cancel = agent.cancel_token();
    agent =
        agent
            .provider(ProgressProvider { ending, cancel })
            .pricing(Pricing::new().with_tiered(
                "progress-model",
                BASE,
                100,
                ModelPrice {
                    input: 2.0,
                    output: 20.0,
                    cache_write: 5.0,
                    cache_write_1h: Some(8.0),
                    cache_read: 0.2,
                },
            ));
    let outcome = agent.run().await.expect("outcome");
    assert_eq!(
        outcome.usage,
        usage(7, 8),
        "latest cumulative progress must be retained once"
    );
    assert!((outcome.cost.expect("known original tier") - 0.000_205_5).abs() < 1e-12);
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert_eq!(outcome.text, "kept");
    assert!(matches!(
        outcome.finish,
        Finish::Stopped { .. } | Finish::Cancelled
    ));
    assert!(agent.messages().iter().all(|message| !matches!(message, stakit_ai_sdk::Message::Assistant(blocks) if blocks.iter().any(|block| matches!(block, stakit_ai_sdk::AssistantContent::ToolUse { .. })))));
}
#[tokio::test]
async fn provider_error_retains_latest_cumulative_progress() {
    assert_partial_progress(Ending::Error).await;
}
#[tokio::test]
async fn eof_retains_latest_cumulative_progress() {
    assert_partial_progress(Ending::Eof).await;
}
#[tokio::test]
async fn cancellation_retains_latest_cumulative_progress() {
    assert_partial_progress(Ending::Cancel).await;
}
