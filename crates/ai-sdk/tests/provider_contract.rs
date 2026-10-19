//! The reusable contract rejects broken backends, not just happy-path fixtures.
#[path = "provider_contract/scripted.rs"]
mod scripted;
use scripted::{ScriptedProvider, end, normal, request, usage};
use stakit_ai_sdk::{
    AssistantContent, CacheStrategy, CacheTarget, CacheTtl, Provider, ProviderError, StopReason,
    StreamEvent, Usage,
    test_support::contract::{
        ErrorStage, assert_completion, assert_completion_error, assert_request_fitting,
        assert_stream, assert_stream_error,
    },
};

#[tokio::test]
async fn normalized_completion_and_stream_pass_the_same_public_contract() {
    let provider = normal();
    assert_completion(
        &provider,
        request(),
        &[AssistantContent::Text("hello".into())],
        &StopReason::EndTurn,
    )
    .await
    .expect("completion contract");
    let events = assert_stream(&provider, request(), &StopReason::EndTurn, &usage())
        .await
        .expect("stream contract");
    assert!(events.contains(&StreamEvent::TextDelta("hello".into())));
}

#[tokio::test]
async fn typed_acquisition_and_streaming_errors_are_supported() {
    for stage in [ErrorStage::Acquisition, ErrorStage::Streaming] {
        let provider = ScriptedProvider {
            failure: Some(stage),
            ..normal()
        };
        assert_stream_error(&provider, request(), stage, |error| {
            matches!(error, ProviderError::Decode { .. })
        })
        .await
        .expect("error contract");
        assert_completion_error(&provider, request(), |error| {
            matches!(error, ProviderError::Decode { .. })
        })
        .await
        .expect("completion error contract");
    }
}

#[tokio::test]
async fn failed_contract_diagnostics_never_echo_provider_content_or_errors() {
    let provider = ScriptedProvider {
        failure: Some(ErrorStage::Acquisition),
        ..normal()
    };
    let error = assert_completion(&provider, request(), &[], &StopReason::EndTurn)
        .await
        .expect_err("failure must be detected");
    assert!(!format!("{error:?} {error}").contains("private"));
    let error = assert_completion(&normal(), request(), &[], &StopReason::EndTurn)
        .await
        .expect_err("content mismatch must be detected");
    assert!(!error.to_string().contains("hello"));
}

#[tokio::test]
async fn broken_stream_protocols_cannot_pass_a_contract() {
    let final_usage = usage();
    for events in [
        vec![],
        vec![StreamEvent::TextDelta("missing end".into())],
        vec![end(), end()],
        vec![end(), StreamEvent::TextDelta("late".into())],
        vec![
            StreamEvent::Start {
                usage: Usage {
                    input_tokens: 100,
                    ..Usage::zero()
                },
            },
            end(),
        ],
        vec![
            StreamEvent::TextDelta("x".into()),
            StreamEvent::Start {
                usage: Usage::zero(),
            },
            end(),
        ],
        vec![StreamEvent::End {
            stop: StopReason::ToolUse,
            usage: final_usage,
        }],
        vec![
            StreamEvent::ToolCall {
                id: String::new(),
                name: "lookup".into(),
                arguments: "{}".into(),
            },
            end(),
        ],
        vec![StreamEvent::End {
            stop: StopReason::EndTurn,
            usage: Usage {
                reasoning_tokens: 99,
                ..final_usage
            },
        }],
    ] {
        let provider = ScriptedProvider {
            events,
            failure: None,
        };
        assert!(
            assert_stream(&provider, request(), &StopReason::EndTurn, &final_usage)
                .await
                .is_err()
        );
    }
}

#[test]
fn fitting_and_cache_checks_use_declared_capabilities_instead_of_vendor_names() {
    let provider = normal();
    let mut original = request();
    original.temperature = Some(0.2);
    original.max_tokens = 200;
    original.cache = CacheStrategy::Breakpoints {
        ttl: CacheTtl::FiveMin,
        points: vec![CacheTarget::System],
    };
    let fitted = assert_request_fitting(&provider, &original).expect("fitting contract");
    assert_eq!(fitted.max_tokens, 16);
    assert_eq!(fitted.temperature, None);
    assert_eq!(fitted.cache, original.cache);
    let capabilities = provider.cache_capabilities_for(&fitted.model);
    assert!(fitted.validate_cache(&capabilities).is_ok());
    let mut unsupported = fitted;
    unsupported.cache = CacheStrategy::Breakpoints {
        ttl: CacheTtl::OneHour,
        points: vec![CacheTarget::System],
    };
    assert!(unsupported.validate_cache(&capabilities).is_err());
}

#[tokio::test]
async fn usage_progress_allows_cache_refinement_but_rejects_lost_counters() {
    let refined = Usage {
        cache_read_tokens: 3,
        ..usage()
    };
    let provider = ScriptedProvider {
        events: vec![
            StreamEvent::Usage {
                usage: Usage {
                    input_tokens: 13,
                    cache_read_tokens: 0,
                    ..usage()
                },
            },
            StreamEvent::Usage { usage: refined },
            StreamEvent::End {
                stop: StopReason::EndTurn,
                usage: refined,
            },
        ],
        failure: None,
    };
    assert_stream(&provider, request(), &StopReason::EndTurn, &refined)
        .await
        .expect("cache refinement is cumulative");
    let provider = ScriptedProvider {
        events: vec![
            StreamEvent::Usage {
                usage: Usage {
                    cache_create_1h_tokens: 3,
                    ..Usage::zero()
                },
            },
            end(),
        ],
        failure: None,
    };
    assert!(
        assert_stream(&provider, request(), &StopReason::EndTurn, &usage())
            .await
            .is_err()
    );
}
