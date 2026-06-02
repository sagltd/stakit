//! End-to-end tests for the agent loop, driven by a mock streaming provider.
#![allow(dead_code)]
// Mock providers satisfy the async `Provider` trait but do no async work.
#![allow(clippy::unused_async_trait_impl)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use futures::StreamExt;
use stakit_ai_sdk::{
    Agent, CancelToken, ChatRequest, ChatResponse, EventStream, FinishReason, LoopEvent, Message,
    Permission, Provider, ProviderError, StopReason, StreamEvent, Usage, tool,
};
use stakit_model::{JsonSchema, Model};

#[derive(serde::Deserialize, Model, JsonSchema)]
struct EchoArgs {
    text: String,
}

/// Echo the text back
#[tool]
async fn echo(args: EchoArgs) -> Result<String, ToolError> {
    Ok(args.text)
}

/// First step asks for a tool call; second step ends the turn with text.
#[derive(Clone)]
struct Mock {
    calls: Arc<AtomicU32>,
    seen: Arc<std::sync::Mutex<Vec<usize>>>,
}

impl Mock {
    fn new() -> Self {
        Self {
            calls: Arc::new(AtomicU32::new(0)),
            seen: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    fn first_seen(&self) -> usize {
        self.seen.lock().unwrap()[0]
    }
}

impl Provider for Mock {
    type Raw = ();

    #[expect(
        clippy::unnecessary_literal_bound,
        reason = "trait ties the lifetime to &self"
    )]
    fn model_id(&self) -> &str {
        "mock"
    }

    async fn complete(&self, _r: ChatRequest) -> Result<ChatResponse<()>, ProviderError> {
        Err(ProviderError::InvalidArgument("unused".into()))
    }

    async fn stream(&self, r: ChatRequest) -> Result<EventStream, ProviderError> {
        self.seen.lock().unwrap().push(r.messages.len());
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let events: Vec<Result<StreamEvent, ProviderError>> = if n == 0 {
            vec![
                Ok(StreamEvent::Start {
                    usage: Usage {
                        input_tokens: 10,
                        ..Usage::default()
                    },
                }),
                Ok(StreamEvent::ToolCall {
                    id: "t1".into(),
                    name: "echo".into(),
                    input: serde_json::json!({ "text": "hi" }),
                }),
                Ok(StreamEvent::End {
                    stop: StopReason::ToolUse,
                    usage: Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                        ..Usage::default()
                    },
                }),
            ]
        } else {
            vec![
                Ok(StreamEvent::TextDelta("done".into())),
                Ok(StreamEvent::End {
                    stop: StopReason::EndTurn,
                    usage: Usage {
                        input_tokens: 20,
                        output_tokens: 3,
                        ..Usage::default()
                    },
                }),
            ]
        };
        Ok(futures::stream::iter(events).boxed())
    }
}

async fn collect(agent: &Agent<Mock, ()>, cancel: CancelToken) -> Vec<LoopEvent> {
    agent
        .run(vec![Message::user_text("hi")], (), cancel)
        .collect()
        .await
}

#[tokio::test]
async fn full_loop_runs_tool_then_ends() {
    let agent = Agent::<Mock, ()>::builder(Mock::new())
        .model("mock")
        .register(echo)
        .build();
    let events = collect(&agent, CancelToken::new()).await;

    let tool_call = events
        .iter()
        .any(|e| matches!(e, LoopEvent::ToolCall { name, .. } if name == "echo"));
    assert!(tool_call, "expected a tool call: {events:?}");

    let tool_result = events.iter().find_map(|e| match e {
        LoopEvent::ToolResult {
            output, is_error, ..
        } => Some((output.clone(), *is_error)),
        _ => None,
    });
    assert_eq!(tool_result, Some((serde_json::json!("hi"), false)));

    let Some(LoopEvent::Done {
        text,
        reason,
        usage,
        ..
    }) = events.last()
    else {
        panic!("last event is not Done: {events:?}");
    };
    assert_eq!(*reason, FinishReason::EndTurn);
    assert_eq!(text, "done");
    // input 10+20, output 5+3
    assert_eq!(usage.input_tokens, 30);
    assert_eq!(usage.output_tokens, 8);
}

#[tokio::test]
async fn denied_tool_yields_error_result() {
    let agent = Agent::<Mock, ()>::builder(Mock::new())
        .model("mock")
        .register(echo)
        .can_use_tool(|name, _input, _cx| {
            Box::pin(async move {
                Permission::Deny {
                    reason: format!("{name} not allowed"),
                }
            })
        })
        .build();
    let events = collect(&agent, CancelToken::new()).await;

    let denied = events.iter().any(|e| {
        matches!(e, LoopEvent::ToolResult { output, is_error: true, .. }
            if output.as_str() == Some("echo not allowed"))
    });
    assert!(denied, "expected a denied tool result: {events:?}");
}

#[tokio::test]
async fn prepare_step_injects_into_history() {
    let provider = Mock::new();
    let agent = Agent::<Mock, ()>::builder(provider.clone())
        .model("mock")
        .register(echo)
        .prepare_step(|step, history| {
            if step == 1 {
                history.push(Message::user_text("injected"));
            }
        })
        .build();
    let _ = collect(&agent, CancelToken::new()).await;
    // Initial "hi" + injected message before the first provider call.
    assert_eq!(provider.first_seen(), 2);
}

#[tokio::test]
async fn run_with_input_drains_injected_messages() {
    let provider = Mock::new();
    let agent = Agent::<Mock, ()>::builder(provider.clone())
        .model("mock")
        .register(echo)
        .build();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tx.send(Message::user_text("from channel")).unwrap();
    drop(tx);
    let _: Vec<LoopEvent> = agent
        .run_with_input(
            vec![Message::user_text("hi")],
            (),
            CancelToken::new(),
            Some(rx),
        )
        .collect()
        .await;
    assert_eq!(provider.first_seen(), 2);
}

/// A provider that asks for two tool calls in one turn, then ends.
#[derive(Clone)]
struct ParallelMock {
    calls: Arc<AtomicU32>,
}

impl stakit_ai_sdk::Provider for ParallelMock {
    type Raw = ();
    #[expect(
        clippy::unnecessary_literal_bound,
        reason = "trait ties the lifetime to &self"
    )]
    fn model_id(&self) -> &str {
        "mock"
    }
    async fn complete(&self, _r: ChatRequest) -> Result<ChatResponse<()>, ProviderError> {
        Err(ProviderError::Cancelled)
    }
    async fn stream(&self, _r: ChatRequest) -> Result<EventStream, ProviderError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let events: Vec<Result<StreamEvent, ProviderError>> = if n == 0 {
            vec![
                Ok(StreamEvent::ToolCall {
                    id: "a".into(),
                    name: "barrier".into(),
                    input: serde_json::json!({}),
                }),
                Ok(StreamEvent::ToolCall {
                    id: "b".into(),
                    name: "barrier".into(),
                    input: serde_json::json!({}),
                }),
                Ok(StreamEvent::End {
                    stop: StopReason::ToolUse,
                    usage: Usage::default(),
                }),
            ]
        } else {
            vec![
                Ok(StreamEvent::TextDelta("done".into())),
                Ok(StreamEvent::End {
                    stop: StopReason::EndTurn,
                    usage: Usage::default(),
                }),
            ]
        };
        Ok(futures::stream::iter(events).boxed())
    }
}

#[derive(serde::Deserialize, Model, JsonSchema)]
struct NoArgs {}

struct BarrierCtx {
    barrier: Arc<tokio::sync::Barrier>,
}

/// Blocks on a shared 2-party barrier — only completes if two instances run
/// concurrently.
#[tool]
async fn barrier(
    cx: &ToolCx<BarrierCtx>,
    _args: NoArgs,
) -> Result<String, stakit_ai_sdk::ToolError> {
    cx.ctx().barrier.wait().await;
    Ok("released".into())
}

#[tokio::test]
async fn tool_calls_in_a_turn_run_concurrently() {
    let agent = Agent::<ParallelMock, BarrierCtx>::builder(ParallelMock {
        calls: Arc::new(AtomicU32::new(0)),
    })
    .model("mock")
    .register(barrier)
    .build();

    let ctx = BarrierCtx {
        barrier: Arc::new(tokio::sync::Barrier::new(2)),
    };
    // If the two tool calls ran sequentially, the first `barrier.wait()` would
    // block forever (count 1/2) and this would time out.
    let run = agent
        .run(vec![Message::user_text("go")], ctx, CancelToken::new())
        .collect::<Vec<_>>();
    let events = tokio::time::timeout(std::time::Duration::from_secs(5), run)
        .await
        .expect("tools must run concurrently (otherwise deadlock)");

    let results = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                LoopEvent::ToolResult {
                    is_error: false,
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        results, 2,
        "both tool calls should produce results: {events:?}"
    );
}

#[test]
fn tools_can_be_added_and_removed_at_runtime() {
    let agent = Agent::<Mock, ()>::builder(Mock::new()).build();
    assert!(agent.tool_names().is_empty());

    agent.register_tool(echo); // add to a live, already-built agent
    assert_eq!(agent.tool_names(), vec!["echo".to_string()]);

    assert!(agent.remove_tool("echo"));
    assert!(agent.tool_names().is_empty());
    assert!(!agent.remove_tool("echo"), "removing twice is false");
}

#[tokio::test]
async fn context_loader_seeds_history() {
    struct Seed;
    impl stakit_ai_sdk::ContextLoader<()> for Seed {
        fn load<'a>(
            &'a self,
            _cx: &'a stakit_ai_sdk::ToolCx<()>,
        ) -> stakit_ai_sdk::BoxFuture<
            'a,
            Result<stakit_ai_sdk::LoadedContext, stakit_ai_sdk::AiError>,
        > {
            Box::pin(async {
                Ok(stakit_ai_sdk::LoadedContext {
                    system: Some("ctx".into()),
                    messages: vec![Message::user_text("seeded")],
                })
            })
        }
    }

    let provider = Mock::new();
    let agent = Agent::<Mock, ()>::builder(provider.clone())
        .model("mock")
        .register(echo)
        .context_loader(Seed)
        .build();
    let _ = collect(&agent, CancelToken::new()).await;
    // seeded message + initial "hi"
    assert_eq!(provider.first_seen(), 2);
}

#[tokio::test]
async fn precancelled_run_finishes_cancelled() {
    let agent = Agent::<Mock, ()>::builder(Mock::new())
        .model("mock")
        .register(echo)
        .build();
    let cancel = CancelToken::new();
    cancel.cancel();
    let events = collect(&agent, cancel).await;
    let Some(LoopEvent::Done { reason, .. }) = events.last() else {
        panic!("no Done event");
    };
    assert_eq!(*reason, FinishReason::Cancelled);
}
