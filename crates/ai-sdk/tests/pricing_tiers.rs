//! Request-size pricing must never reprice earlier steps in a run.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use futures::future::BoxFuture;
use stakit_ai_sdk::{
    Agent, AgentCx, AgentError, AgentMiddleware, ChatRequest, ChatResponse, EventStream, Flow,
    Message, ModelPrice, Pricing, Provider, ProviderError, StopReason, StreamEvent, Usage,
    event_stream, tool,
};

const BASE: ModelPrice = ModelPrice {
    input: 0.10,
    output: 0.50,
    cache_read: 0.01,
    cache_write: 0.125,
    cache_write_1h: None,
};
const LONG: ModelPrice = ModelPrice {
    input: 0.50,
    output: 2.50,
    cache_read: 0.05,
    cache_write: 0.625,
    cache_write_1h: None,
};

fn pricing() -> Pricing {
    Pricing::new().with_tiered("tiered", BASE, 100_000, LONG)
}

fn assert_cost(usage: Usage, expected: f64) {
    let actual = pricing().cost("tiered", &usage).unwrap();
    assert!((actual - expected).abs() < 1e-12, "{actual} != {expected}");
}

#[test]
fn exact_threshold_uses_base_rates_for_input_and_output() {
    assert_cost(
        Usage {
            input_tokens: 100_000,
            output_tokens: 1000,
            ..Usage::zero()
        },
        0.0105,
    );
}

#[test]
fn first_token_above_threshold_reprices_the_whole_request() {
    assert_cost(
        Usage {
            input_tokens: 100_001,
            output_tokens: 1000,
            ..Usage::zero()
        },
        0.052_500_5,
    );
}

#[test]
fn cache_reads_and_writes_both_count_toward_the_threshold() {
    let usage = Usage {
        cache_read_tokens: 60_000,
        cache_create_tokens: 40_000,
        output_tokens: 1000,
        ..Usage::zero()
    };
    assert_cost(usage, 0.0061);
    assert_cost(
        Usage {
            input_tokens: 1,
            ..usage
        },
        0.030_500_5,
    );
}

#[test]
fn zero_and_unknown_costs_preserve_existing_contract() {
    assert_cost(Usage::zero(), 0.0);
    assert_eq!(pricing().cost("unknown", &Usage::zero()), None);
    assert_eq!(pricing().price("tiered"), Some(BASE));
}

#[test]
#[expect(
    clippy::cast_precision_loss,
    reason = "upper-bound cost oracle compares within relative tolerance"
)]
fn overflowing_prompt_counts_still_select_the_long_tier() {
    let usage = Usage {
        input_tokens: u64::MAX,
        cache_read_tokens: u64::MAX,
        ..Usage::zero()
    };
    let expected = u64::MAX as f64 / 1_000_000.0 * (LONG.input + LONG.cache_read);
    let actual = pricing().cost("tiered", &usage).unwrap();
    assert!((actual / expected - 1.0).abs() < 1e-12);
}

#[tool(name = "next_step", description = "Continue the scripted test.")]
fn next_step() -> Result<&'static str, ToolError> {
    Ok("ok")
}

struct TwoRequests {
    model: String,
    calls: Arc<AtomicUsize>,
    first: u64,
    second: u64,
}

impl Provider for TwoRequests {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn complete(
        &self,
        _request: ChatRequest,
    ) -> BoxFuture<'_, Result<ChatResponse, ProviderError>> {
        Box::pin(async { Err(ProviderError::Cancelled) })
    }

    fn stream(&self, _request: ChatRequest) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
        let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
        let input_tokens = if first { self.first } else { self.second };
        Box::pin(async move {
            let mut events = Vec::new();
            if first {
                events.push(Ok(StreamEvent::ToolCall {
                    id: "call-1".into(),
                    name: "next_step".into(),
                    arguments: "{}".into(),
                }));
            } else {
                events.push(Ok(StreamEvent::TextDelta("done".into())));
            }
            events.push(Ok(StreamEvent::End {
                stop: if first {
                    StopReason::ToolUse
                } else {
                    StopReason::EndTurn
                },
                usage: Usage {
                    input_tokens,
                    ..Usage::zero()
                },
            }));
            Ok(event_stream(events))
        })
    }
}

async fn run_cost(first: u64, second: u64) -> f64 {
    let mut agent = Agent::new(())
        .provider(TwoRequests {
            model: "tiered".to_owned(),
            calls: Arc::default(),
            first,
            second,
        })
        .pricing(pricing())
        .register_tool(next_step)
        .with_context(vec![Message::user("go")]);
    agent.run().await.unwrap().cost.unwrap()
}

#[tokio::test]
async fn two_short_requests_never_trigger_a_long_prompt_rate_on_accumulated_usage() {
    assert!((run_cost(60_000, 60_000).await - 0.012).abs() < 1e-12);
}

#[tokio::test]
async fn a_short_request_after_a_long_one_keeps_its_base_rate() {
    assert!((run_cost(120_000, 40_000).await - 0.064).abs() < 1e-12);
}

struct SwitchAfterTool;

#[async_trait::async_trait]
impl AgentMiddleware<()> for SwitchAfterTool {
    async fn on_step(&self, cx: &mut AgentCx<'_, ()>) -> Result<Flow, AgentError> {
        if cx.index() > 0 {
            cx.set_model("second");
        }
        Ok(Flow::Continue)
    }
}

async fn switched_cost(prices: Pricing) -> Option<f64> {
    let calls = Arc::default();
    let mut agent = Agent::new(())
        .provider(TwoRequests {
            model: "first".to_owned(),
            calls: Arc::clone(&calls),
            first: 60_000,
            second: 40_000,
        })
        .register_provider(TwoRequests {
            model: "second".to_owned(),
            calls,
            first: 60_000,
            second: 40_000,
        })
        .pricing(prices)
        .register_tool(next_step)
        .register_middleware(SwitchAfterTool)
        .with_context(vec![Message::user("go")]);
    agent.run().await.unwrap().cost
}

#[tokio::test]
async fn switching_models_preserves_each_requests_original_price() {
    let first = ModelPrice {
        input: 1.0,
        output: 0.0,
        cache_read: 0.0,
        cache_write: 0.0,
        cache_write_1h: None,
    };
    let second = ModelPrice {
        input: 2.0,
        ..first
    };
    let cost = switched_cost(Pricing::new().with("first", first).with("second", second))
        .await
        .unwrap();
    assert!((cost - 0.14).abs() < 1e-12);
}

#[tokio::test]
async fn a_consumed_unpriced_request_keeps_the_total_unknown_after_a_model_switch() {
    let second = ModelPrice {
        input: 2.0,
        output: 0.0,
        cache_read: 0.0,
        cache_write: 0.0,
        cache_write_1h: None,
    };
    assert_eq!(
        switched_cost(Pricing::new().with("second", second)).await,
        None
    );
}

struct Interrupted;

impl Provider for Interrupted {
    #[expect(
        clippy::unnecessary_literal_bound,
        reason = "Provider trait returns a borrowed model id"
    )]
    fn model_id(&self) -> &str {
        "tiered"
    }
    fn complete(
        &self,
        _request: ChatRequest,
    ) -> BoxFuture<'_, Result<ChatResponse, ProviderError>> {
        Box::pin(async { Err(ProviderError::Cancelled) })
    }
    fn stream(&self, _request: ChatRequest) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
        Box::pin(async {
            Ok(event_stream(vec![
                Ok(StreamEvent::Start {
                    usage: Usage {
                        input_tokens: 100_001,
                        output_tokens: 1000,
                        ..Usage::zero()
                    },
                }),
                Err(ProviderError::Transport("interrupted".to_owned())),
            ]))
        })
    }
}

#[tokio::test]
async fn an_interrupted_request_is_metered_at_its_own_long_prompt_rate() {
    let mut agent = Agent::new(())
        .provider(Interrupted)
        .pricing(pricing())
        .with_context(vec![Message::user("go")]);
    let cost = agent.run().await.unwrap().cost.unwrap();
    assert!((cost - 0.052_500_5).abs() < 1e-12);
}
