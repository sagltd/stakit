use futures::StreamExt as _;
use hashbrown::HashSet;

use super::{ContractViolation, assert_usage, bounded};
use crate::{ChatRequest, Provider, ProviderError, StopReason, StreamEvent, Usage};

/// Prevent an always-ready infinite fixture from starving the timeout timer.
const MAX_EVENTS: usize = 4096;

/// Where a scripted failure must be observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorStage {
    /// The provider rejects before returning a stream.
    Acquisition,
    /// The returned stream reports an error before any normal terminal event.
    Streaming,
}

const fn usage_contains(final_usage: &Usage, partial: &Usage) -> bool {
    final_usage
        .input_tokens
        .saturating_add(final_usage.cache_create_tokens)
        .saturating_add(final_usage.cache_read_tokens)
        >= partial
            .input_tokens
            .saturating_add(partial.cache_create_tokens)
            .saturating_add(partial.cache_read_tokens)
        && final_usage.output_tokens >= partial.output_tokens
        && final_usage.cache_create_tokens >= partial.cache_create_tokens
        && final_usage.cache_create_1h_tokens >= partial.cache_create_1h_tokens
        && final_usage.cache_read_tokens >= partial.cache_read_tokens
        && final_usage.reasoning_tokens >= partial.reasoning_tokens
}

/// Validate normal stream termination and cumulative usage using a deterministic fixture.
///
/// Start is optional; if present it must precede payload events. Tool argument strings
/// are preserved as received, including malformed JSON for the agent to handle.
/// Complete replay blocks, when supplied, must have unique contiguous positions.
///
/// # Errors
/// Returns a payload-safe violation for stream errors, missing/extra terminal events,
/// invalid tool IDs/usage/replay positions or a fixture exceeding 4096 events/five seconds.
pub async fn assert_stream(
    provider: &dyn Provider,
    request: ChatRequest,
    stop: &StopReason,
    usage: &Usage,
) -> Result<Vec<StreamEvent>, ContractViolation> {
    bounded(async {
        let mut stream = provider
            .stream(request)
            .await
            .map_err(|_| ContractViolation("stream acquisition unexpectedly failed"))?;
        let mut events = Vec::new();
        let mut start = None;
        let mut has_ended = false;
        let mut tool_ids = HashSet::new();
        let mut block_indices = HashSet::new();
        let mut has_tool = false;
        while let Some(event) = stream.next().await {
            if events.len() >= MAX_EVENTS || has_ended {
                return Err(ContractViolation(
                    "stream exceeds fixture limit or continues after End",
                ));
            }
            let event =
                event.map_err(|_| ContractViolation("normal stream unexpectedly failed"))?;
            match &event {
                StreamEvent::Start { usage } => {
                    if !events.is_empty() {
                        return Err(ContractViolation("Start is repeated or follows content"));
                    }
                    assert_usage(usage)?;
                    start = Some(*usage);
                }
                StreamEvent::Usage { usage } => {
                    assert_usage(usage)?;
                    if start.is_some_and(|partial| !usage_contains(usage, &partial)) {
                        return Err(ContractViolation("stream usage progress is not cumulative"));
                    }
                    start = Some(*usage);
                }
                StreamEvent::ToolCall { id, name, .. } => {
                    if id.is_empty() || name.is_empty() || !tool_ids.insert(id.clone()) {
                        return Err(ContractViolation(
                            "tool-call IDs/names must be nonempty and IDs unique",
                        ));
                    }
                    has_tool = true;
                }
                StreamEvent::AssistantBlock { index, .. } => {
                    if !block_indices.insert(*index) {
                        return Err(ContractViolation("complete replay positions repeat"));
                    }
                }
                StreamEvent::End {
                    stop: actual_stop,
                    usage: actual_usage,
                } => {
                    assert_usage(actual_usage)?;
                    if actual_stop != stop
                        || actual_usage != usage
                        || start.is_some_and(|partial| !usage_contains(actual_usage, &partial))
                    {
                        return Err(ContractViolation(
                            "terminal stop/usage differs or usage is not cumulative",
                        ));
                    }
                    if *actual_stop == StopReason::ToolUse && !has_tool {
                        return Err(ContractViolation("tool-use stream contains no tool call"));
                    }
                    has_ended = true;
                }
                StreamEvent::TextDelta(_)
                | StreamEvent::ReasoningDelta(_)
                | StreamEvent::SignatureDelta(_) => {}
            }
            events.push(event);
        }
        if !has_ended {
            return Err(ContractViolation("stream ended without End"));
        }
        for index in 0..block_indices.len() {
            let index =
                u64::try_from(index).map_err(|_| ContractViolation("invalid replay index"))?;
            if !block_indices.contains(&index) {
                return Err(ContractViolation("complete replay positions contain a gap"));
            }
        }
        if crate::agent::validate_stream_replay(&events).is_some() {
            return Err(ContractViolation(
                "stream replay violates the agent protocol",
            ));
        }
        Ok(events)
    })
    .await?
}

/// Check acquisition/streaming errors stay typed and never become a successful End.
/// The stream must close after its error; error payloads never appear in diagnostics.
///
/// # Errors
/// Returns a violation for the wrong failure stage/category, a successful terminal,
/// continued events after an error or an unbounded fixture.
pub async fn assert_stream_error(
    provider: &dyn Provider,
    request: ChatRequest,
    stage: ErrorStage,
    matches: impl Fn(&ProviderError) -> bool,
) -> Result<(), ContractViolation> {
    bounded(async {
        let mut stream = match provider.stream(request).await {
            Err(error) if stage == ErrorStage::Acquisition && matches(&error) => return Ok(()),
            Err(_) => {
                return Err(ContractViolation(
                    "stream failed at the wrong stage or with the wrong error",
                ));
            }
            Ok(_) if stage == ErrorStage::Acquisition => {
                return Err(ContractViolation(
                    "expected acquisition error but stream opened",
                ));
            }
            Ok(stream) => stream,
        };
        for _ in 0..MAX_EVENTS {
            match stream.next().await {
                Some(Err(error)) if matches(&error) => {
                    return if stream.next().await.is_none() {
                        Ok(())
                    } else {
                        Err(ContractViolation("stream continued after error"))
                    };
                }
                Some(Err(_) | Ok(StreamEvent::End { .. })) | None => {
                    return Err(ContractViolation(
                        "stream did not return the expected error before End",
                    ));
                }
                Some(Ok(_)) => {}
            }
        }
        Err(ContractViolation("error fixture exceeded event limit"))
    })
    .await?
}
