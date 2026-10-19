//! What each Claude model accepts, read from its id.
//!
//! Ids are parsed into a family and version (`claude-opus-5-5`,
//! `claude-haiku-4-5-20251001`, `claude-3-7-sonnet-20250219`,
//! `anthropic.claude-sonnet-5-5`, `claude-opus-4-5@20251101`). Unknown or newer
//! ids get the newest, strictest rules, so a model that dropped a field never
//! receives it.

use crate::provider::{ProviderCapabilities, ThinkingSupport};

/// How [`ThinkingConfig::Off`](crate::ThinkingConfig::Off) is sent to a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ThinkingOff {
    /// Omit `thinking`: the model thinks only when asked — or it always thinks
    /// and cannot be turned off (Opus 5.5, Fable, Mythos).
    Omit,
    /// `{"type": "disabled"}`: omitting the field would run adaptive thinking.
    Disabled,
    /// `{"type": "between_tools"}`: Sonnet 5.5's lowest setting (`disabled` is a 400).
    BetweenTools,
}

/// What one Claude model accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ModelRules {
    /// Capabilities the request is fitted to.
    pub(super) capabilities: ProviderCapabilities,
    /// The wire form of thinking-off.
    pub(super) thinking_off: ThinkingOff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Opus,
    Sonnet,
    Haiku,
    Fable,
    Mythos,
}

/// `(major, minor)`.
type Version = (u32, u32);

/// Output cap of every Claude model from the 4.6 generation on.
const LARGE_OUTPUT: Option<u32> = Some(128_000);

const fn rules(
    forced_tool_choice: bool,
    thinking: ThinkingSupport,
    sampling_params: bool,
    max_output_tokens: Option<u32>,
    thinking_off: ThinkingOff,
) -> ModelRules {
    ModelRules {
        capabilities: ProviderCapabilities {
            forced_tool_choice,
            thinking,
            sampling_params,
            prompt_cache: true,
            vision: true,
            max_output_tokens,
        },
        thinking_off,
    }
}

/// Claude 4.5 and older (Haiku 4.5 included): budget thinking, sampling, forced
/// tools; no thinking unless asked.
const LEGACY: ModelRules = rules(true, ThinkingSupport::Budget, true, None, ThinkingOff::Omit);
/// Opus / Sonnet 4.6: adaptive thinking, sampling still accepted.
const ADAPTIVE_WITH_SAMPLING: ModelRules = rules(
    true,
    ThinkingSupport::Adaptive,
    true,
    LARGE_OUTPUT,
    ThinkingOff::Omit,
);
/// Opus 4.7 / 4.8: sampling parameters removed; no thinking unless asked.
const ADAPTIVE: ModelRules = rules(
    true,
    ThinkingSupport::Adaptive,
    false,
    LARGE_OUTPUT,
    ThinkingOff::Omit,
);
/// Opus 5, Sonnet 5, Haiku 5.5: adaptive thinking runs unless `disabled`.
const THINKING_BY_DEFAULT: ModelRules = rules(
    true,
    ThinkingSupport::Adaptive,
    false,
    LARGE_OUTPUT,
    ThinkingOff::Disabled,
);
/// Sonnet 5.5: no forced tool choice; `between_tools` is its thinking-off.
const SONNET_5_5: ModelRules = rules(
    false,
    ThinkingSupport::Adaptive,
    false,
    LARGE_OUTPUT,
    ThinkingOff::BetweenTools,
);
/// Fable 5 / Mythos 5: thinking always on, forced tool choice still accepted.
const ALWAYS_THINKING: ModelRules = rules(
    true,
    ThinkingSupport::Adaptive,
    false,
    LARGE_OUTPUT,
    ThinkingOff::Omit,
);
/// Opus 5.5, Fable / Mythos 5.1, and anything newer or unknown: thinking
/// always on, no forced tool choice, no sampling.
const NEWEST: ModelRules = rules(
    false,
    ThinkingSupport::Adaptive,
    false,
    LARGE_OUTPUT,
    ThinkingOff::Omit,
);

fn parse_family(token: &str) -> Option<Family> {
    match token {
        "opus" => Some(Family::Opus),
        "sonnet" => Some(Family::Sonnet),
        "haiku" => Some(Family::Haiku),
        "fable" => Some(Family::Fable),
        "mythos" => Some(Family::Mythos),
        _ => None,
    }
}

/// A one- or two-digit version number (`20251001` is a date, not a version).
fn version_part(token: &str) -> Option<u32> {
    if token.len() > 2 {
        return None;
    }
    token.parse().ok()
}

/// The family and version in a model id, if it names one.
fn parse(model: &str) -> Option<(Family, Version)> {
    let (_, rest) = model.split_once("claude-")?;
    // Vertex pins dated snapshots as `claude-opus-4-5@20251101`.
    let rest = rest.split('@').next().unwrap_or(rest);
    let family = rest.split('-').find_map(parse_family)?;
    let mut numbers = rest.split('-').filter_map(version_part);
    let major = numbers.next()?;
    Some((family, (major, numbers.next().unwrap_or(0))))
}

/// The rules for `model`.
pub(super) fn rules_for(model: &str) -> ModelRules {
    let Some((family, version)) = parse(model) else {
        return NEWEST;
    };
    match family {
        Family::Opus => match version {
            v if v < (4, 6) => LEGACY,
            v if v < (4, 7) => ADAPTIVE_WITH_SAMPLING,
            v if v < (5, 0) => ADAPTIVE,
            v if v < (5, 5) => THINKING_BY_DEFAULT,
            _ => NEWEST,
        },
        Family::Sonnet => match version {
            v if v < (4, 6) => LEGACY,
            v if v < (5, 0) => ADAPTIVE_WITH_SAMPLING,
            v if v < (5, 5) => THINKING_BY_DEFAULT,
            (5, 5) => SONNET_5_5,
            _ => NEWEST,
        },
        Family::Haiku => match version {
            v if v < (5, 0) => LEGACY,
            v if v <= (5, 5) => THINKING_BY_DEFAULT,
            _ => NEWEST,
        },
        Family::Fable | Family::Mythos => match version {
            v if v < (5, 1) => ALWAYS_THINKING,
            _ => NEWEST,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_parse_into_family_and_version() {
        let cases = [
            ("claude-opus-5-5", Some((Family::Opus, (5, 5)))),
            ("claude-haiku-4-5-20251001", Some((Family::Haiku, (4, 5)))),
            ("claude-sonnet-4-20250514", Some((Family::Sonnet, (4, 0)))),
            ("claude-3-7-sonnet-20250219", Some((Family::Sonnet, (3, 7)))),
            (
                "anthropic.claude-sonnet-5-5",
                Some((Family::Sonnet, (5, 5))),
            ),
            (
                "us.anthropic.claude-opus-5-5-v1:0",
                Some((Family::Opus, (5, 5))),
            ),
            ("claude-opus-4-5@20251101", Some((Family::Opus, (4, 5)))),
            ("claude-fable-5-1", Some((Family::Fable, (5, 1)))),
            ("claude-opus-5", Some((Family::Opus, (5, 0)))),
            ("gpt-4o", None),
            ("claude-unknown-9", None),
        ];
        for (model, expected) in cases {
            assert_eq!(parse(model), expected, "{model}");
        }
    }

    #[test]
    fn each_generation_gets_its_rules() {
        let cases = [
            ("claude-haiku-4-5-20251001", LEGACY),
            ("claude-sonnet-4-5", LEGACY),
            ("claude-3-7-sonnet-20250219", LEGACY),
            ("claude-opus-4-6", ADAPTIVE_WITH_SAMPLING),
            ("claude-sonnet-4-6", ADAPTIVE_WITH_SAMPLING),
            ("claude-opus-4-8", ADAPTIVE),
            ("claude-opus-5", THINKING_BY_DEFAULT),
            ("claude-sonnet-5", THINKING_BY_DEFAULT),
            ("claude-haiku-5-5", THINKING_BY_DEFAULT),
            ("claude-sonnet-5-5", SONNET_5_5),
            ("claude-opus-5-5", NEWEST),
            ("claude-fable-5", ALWAYS_THINKING),
            ("claude-fable-5-1", NEWEST),
            ("claude-mythos-5-1", NEWEST),
            ("claude-sonnet-7", NEWEST),
            ("not-a-claude-id", NEWEST),
        ];
        for (model, expected) in cases {
            assert_eq!(rules_for(model), expected, "{model}");
        }
    }
}
