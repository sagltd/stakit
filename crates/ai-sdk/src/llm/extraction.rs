//! Usage-preserving structured extraction with payload-free failures.

use std::fmt;

use serde::de::DeserializeOwned;
use stakit_model::{JsonSchema, Validate};

use super::LLM;
use crate::{AssistantContent, Provider, StopReason, Usage};

/// Outcome of exactly one attempted structured extraction.
///
/// Record known usage against `model` before inspecting or applying `result`.
/// `None` means no tally was returned, not that the request was free. A preflight
/// failure made no provider call; a provider failure may have incurred cost.
/// Dropping the future before it completes cannot yield an accounting outcome.
#[must_use = "record observed usage before inspecting or applying the extraction result"]
pub struct Extraction<T> {
    /// Exact request model, including an override. On preflight failure this is
    /// the selected model for the request that could not be sent.
    pub model: String,
    /// Normalized usage from a returned response, including observed zero.
    /// No usage is invented for preflight or provider failures.
    pub usage: Option<Usage>,
    /// Decoded and validated value, or a payload-free failure category.
    pub result: Result<T, ExtractionError>,
}

// Extracted values may contain personal data even when their type is Debug.
impl<T> fmt::Debug for Extraction<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Extraction")
            .field("model", &self.model)
            .field("usage", &self.usage)
            .field("result", &self.result.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

/// Payload-free extraction failure categories. Neither formatting nor the error
/// source exposes provider messages, raw tool arguments or validation text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ExtractionError {
    /// No user message was supplied; no provider call was made.
    #[error("extraction user message is missing")]
    MissingUser,
    /// Request preflight failed; no provider call was made.
    #[error("extraction request is invalid")]
    InvalidRequest,
    /// The provider returned an error without normalized response usage.
    #[error("extraction provider call failed")]
    ProviderFailed,
    /// The response explicitly refused the request.
    #[error("extraction was refused")]
    Refused,
    /// The response did not finish with an accepted terminal reason.
    #[error("extraction response is incomplete")]
    Incomplete,
    /// No actual extract tool-use block was returned.
    #[error("extraction tool result is missing")]
    MissingTool,
    /// The tool input could not be deserialized, or multiple tool-use blocks
    /// made the structured result ambiguous.
    #[error("extraction payload is invalid")]
    InvalidPayload,
    /// The deserialized value failed its declared validation rules.
    #[error("extraction validation failed")]
    ValidationFailed,
}

/// Shares terminal and actual-tool checks with the legacy extraction helper.
pub(super) fn tool_input(
    stop: &StopReason,
    content: Vec<AssistantContent>,
    require_single_tool: bool,
) -> Result<serde_json::Value, ExtractionError> {
    match stop {
        StopReason::Refusal => return Err(ExtractionError::Refused),
        StopReason::ToolUse | StopReason::EndTurn => {}
        _ => return Err(ExtractionError::Incomplete),
    }
    if require_single_tool
        && content
            .iter()
            .filter(|block| matches!(block, AssistantContent::ToolUse { .. }))
            .count()
            > 1
    {
        return Err(ExtractionError::InvalidPayload);
    }
    content
        .into_iter()
        .find_map(|block| match block {
            AssistantContent::ToolUse { name, input, .. } if &*name == "extract" => Some(input),
            _ => None,
        })
        .ok_or(ExtractionError::MissingTool)
}

impl<P: Provider> LLM<P> {
    /// Performs one extraction, retaining accounting metadata on invalid results.
    ///
    /// Unlike [`LLM::extract`], this method also runs [`Validate::validate`]. It
    /// requires exactly one tool-use block, named `extract`, and makes no
    /// retries or additional model calls. Meter `usage` against `model`
    /// before handling `result`; unknown usage needs an explicit caller policy.
    /// A returned tally is the provider adapter's normalized observation, not
    /// independent verification of the provider's wire counters. Malformed wire
    /// responses returned as provider errors cannot expose usage through the
    /// current [`Provider::complete`] contract.
    ///
    /// The callback below demonstrates accounting-before-result ordering; the
    /// application must durably record accounting and handle unknown usage.
    ///
    /// ```
    /// use stakit_ai_sdk::{ExtractionError, LLM, Provider, Usage};
    /// #[derive(serde::Deserialize, stakit_model::Model, stakit_model::JsonSchema)]
    /// struct Fact { text: String }
    /// async fn extract<P: Provider>(
    ///     provider: P,
    ///     record: impl FnOnce(&str, Option<Usage>),
    /// ) -> Result<String, ExtractionError> {
    ///     let outcome = LLM::new(provider).user("The project is named Dawn.")
    ///         .extract_with_usage::<Fact>().await;
    ///     record(&outcome.model, outcome.usage);
    ///     let fact = outcome.result?;
    ///     Ok(fact.text)
    /// }
    /// ```
    pub async fn extract_with_usage<T: JsonSchema + DeserializeOwned + Validate>(
        self,
    ) -> Extraction<T> {
        let selected_model = self
            .model
            .clone()
            .unwrap_or_else(|| self.provider.model_id().to_owned());
        if self.user.is_none() {
            return Extraction {
                model: selected_model,
                usage: None,
                result: Err(ExtractionError::MissingUser),
            };
        }
        let Ok(request) = self.extraction_request::<T>() else {
            return Extraction {
                model: selected_model,
                usage: None,
                result: Err(ExtractionError::InvalidRequest),
            };
        };
        let model = request.model.clone();
        let Ok(response) = self.provider.complete(request).await else {
            return Extraction {
                model,
                usage: None,
                result: Err(ExtractionError::ProviderFailed),
            };
        };
        let usage = Some(response.usage);
        let result = tool_input(&response.stop, response.content, true)
            .and_then(|input| {
                serde_json::from_value::<T>(input).map_err(|_| ExtractionError::InvalidPayload)
            })
            .and_then(|value| {
                value
                    .validate()
                    .map_err(|_| ExtractionError::ValidationFailed)?;
                Ok(value)
            });
        Extraction {
            model,
            usage,
            result,
        }
    }
}
