//! Request-size pricing must never reprice earlier steps in a run.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use stakit_ai_sdk::{
    Agent, AgentCx, AgentError, AgentMiddleware, Flow, Message, ModelPrice, Pricing, ProviderError,
    StreamEvent, Usage, event_stream, tool,
};

#[path = "support/mock.rs"]
mod mock;
use mock::{Scripted, stream_of, text_turn, tool_turn};

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

/// Calls `next_step` on the run's first request (counted in `calls`, which
/// providers may share), then answers; each request reports its own input.
fn two_requests(
    model: &'static str,
    calls: &Arc<AtomicUsize>,
    first: u64,
    second: u64,
) -> Scripted {
    let calls = Arc::clone(calls);
    Scripted::new(model, move |_, _| {
        let is_first = calls.fetch_add(1, Ordering::SeqCst) == 0;
        let usage = Usage {
            input_tokens: if is_first { first } else { second },
            ..Usage::zero()
        };
        let mut events = if is_first {
            tool_turn("call-1", "next_step", "{}")
        } else {
            text_turn("done")
        };
        if let Some(StreamEvent::End { usage: end, .. }) = events.last_mut() {
            *end = usage;
        }
        Ok(stream_of(events))
    })
}

async fn run_cost(first: u64, second: u64) -> f64 {
    let mut agent = Agent::new(())
        .provider(two_requests("tiered", &Arc::default(), first, second))
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
        .provider(two_requests("first", &calls, 60_000, 40_000))
        .register_provider(two_requests("second", &calls, 60_000, 40_000))
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

/// Reports long-prompt progress, then fails mid-stream.
fn interrupted() -> Scripted {
    Scripted::new("tiered", |_, _| {
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

#[tokio::test]
async fn an_interrupted_request_is_metered_at_its_own_long_prompt_rate() {
    let mut agent = Agent::new(())
        .provider(interrupted())
        .pricing(pricing())
        .with_context(vec![Message::user("go")]);
    let cost = agent.run().await.unwrap().cost.unwrap();
    assert!((cost - 0.052_500_5).abs() < 1e-12);
}
