//! A live tool round trip shared by the per-vendor live release checks.
//! Include with `#[path = "support/live.rs"] mod live;`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use stakit_ai_sdk::{Agent, Finish, Message, Provider, tool};

/// The live tool's context: the status it reports and how often it ran.
pub(crate) struct Status {
    value: &'static str,
    calls: AtomicUsize,
}

#[tool(name = "lookup_status", description = "Read the current status once.")]
fn lookup_status(cx: &stakit_ai_sdk::ToolCx<Arc<Status>>) -> Result<&'static str, ToolError> {
    cx.ctx().calls.fetch_add(1, Ordering::SeqCst);
    Ok(cx.ctx().value)
}

/// The model must call `lookup_status` exactly once and answer with `value`.
pub(crate) async fn assert_status_round_trip(
    provider: impl Provider + 'static,
    value: &'static str,
) {
    let status = Arc::new(Status {
        value,
        calls: AtomicUsize::new(0),
    });
    let mut agent = Agent::new(Arc::clone(&status))
        .provider(provider)
        .system("Use lookup_status exactly once to read the current status, then return its exact value. Never guess it.")
        .register_tool(lookup_status)
        .max_tokens(1024)
        .with_context(vec![Message::user("Read the current status and tell me its value.")]);
    let outcome = agent.run().await.expect("live tool round trip");
    assert!(
        matches!(outcome.finish, Finish::EndTurn),
        "{:?}",
        outcome.finish
    );
    assert_eq!(status.calls.load(Ordering::SeqCst), 1);
    assert!(outcome.text.contains(value), "{}", outcome.text);
}
