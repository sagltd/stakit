//! Payload-safe, bounded assertions for deterministic provider fixtures.
//!
//! Implementors supply requests and expected normalized results; the suite has
//! no vendor IDs, live credentials, HTTP dependency or model-specific limits.
//! It checks protocol promises, not whether a model gives a useful answer.

use std::{future::Future, time::Duration};

use crate::{
    AssistantContent, ChatRequest, ChatResponse, Provider, ProviderError, StopReason, Usage,
};

mod stream;
pub use stream::{ErrorStage, assert_stream, assert_stream_error};

/// A failed provider-contract invariant. Diagnostics never include provider payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("provider contract: {0}")]
pub struct ContractViolation(pub(super) &'static str);

/// Offline probes must terminate even when a custom provider stalls.
const TIMEOUT: Duration = Duration::from_secs(5);

async fn bounded<T>(future: impl Future<Output = T>) -> Result<T, ContractViolation> {
    tokio::time::timeout(TIMEOUT, future)
        .await
        .map_err(|_| ContractViolation("fixture did not terminate within five seconds"))
}

const fn assert_usage(usage: &Usage) -> Result<(), ContractViolation> {
    if usage.reasoning_tokens > usage.output_tokens {
        return Err(ContractViolation(
            "reasoning must be a subset of output usage",
        ));
    }
    if usage.cache_create_1h_tokens > usage.cache_create_tokens {
        return Err(ContractViolation(
            "one-hour cache writes must be a subset of total writes",
        ));
    }
    Ok(())
}

/// Complete one scripted request and compare its normalized content and stop reason.
/// The response is returned so a fixture can assert its exact usage independently.
///
/// # Errors
/// Returns a payload-safe violation for errors, timeouts, output mismatch or invalid usage.
pub async fn assert_completion(
    provider: &dyn Provider,
    request: ChatRequest,
    content: &[AssistantContent],
    stop: &StopReason,
) -> Result<ChatResponse, ContractViolation> {
    let response = bounded(provider.complete(request))
        .await?
        .map_err(|_| ContractViolation("completion unexpectedly failed"))?;
    if response.content != content || &response.stop != stop {
        return Err(ContractViolation(
            "completion content or stop reason differs",
        ));
    }
    assert_usage(&response.usage)?;
    if response.stop == StopReason::ToolUse
        && !response
            .content
            .iter()
            .any(|block| matches!(block, AssistantContent::ToolUse { .. }))
    {
        return Err(ContractViolation(
            "tool-use completion contains no tool call",
        ));
    }
    Ok(response)
}

/// Check a scripted completion fails with the expected typed error category.
///
/// # Errors
/// Returns a payload-safe violation if the completion succeeds, hangs or has the wrong error.
pub async fn assert_completion_error(
    provider: &dyn Provider,
    request: ChatRequest,
    matches: impl Fn(&ProviderError) -> bool,
) -> Result<(), ContractViolation> {
    match bounded(provider.complete(request)).await? {
        Err(error) if matches(&error) => Ok(()),
        _ => Err(ContractViolation(
            "completion did not return the expected error",
        )),
    }
}

/// Check shared capability fitting is idempotent and preserves conversation meaning.
///
/// Returns the fitted request for provider-specific assertions about supported controls.
/// Cache acceptance is tested separately through the declared cache-capability API.
///
/// # Errors
/// Returns a violation when fitting changes conversation data or is not idempotent.
pub fn assert_request_fitting(
    provider: &dyn Provider,
    request: &ChatRequest,
) -> Result<ChatRequest, ContractViolation> {
    let capabilities = provider.capabilities_for(&request.model);
    let mut fitted = request.clone();
    fitted.fit_to(&capabilities);
    if fitted.model != request.model
        || fitted.messages != request.messages
        || fitted.system != request.system
    {
        return Err(ContractViolation(
            "capability fitting changed conversation data",
        ));
    }
    let mut twice = fitted.clone();
    twice.fit_to(&capabilities);
    if fitted != twice {
        return Err(ContractViolation("capability fitting is not idempotent"));
    }
    Ok(fitted)
}
