//! Shared cache declarations distinguish implicit behavior from explicit controls.
use stakit_ai_sdk::{
    CacheCapabilities, CacheStrategy, CacheTarget, CacheTtl, ChatRequest,
    ExplicitCacheCapabilities, ProviderError,
};

const fn explicit(ttls: Vec<CacheTtl>) -> CacheCapabilities {
    CacheCapabilities::Known {
        implicit: false,
        explicit: Some(ExplicitCacheCapabilities {
            max_points: 4,
            ttls,
        }),
    }
}
fn requested(ttl: CacheTtl) -> CacheStrategy {
    CacheStrategy::Breakpoints {
        ttl,
        points: vec![CacheTarget::System],
    }
}
fn rejects(capabilities: &CacheCapabilities, strategy: &CacheStrategy) {
    assert!(matches!(
        capabilities.validate(strategy),
        Err(ProviderError::InvalidArgument(_))
    ));
}
#[test]
fn undeclared_explicit_cache_is_rejected() {
    rejects(&CacheCapabilities::Unknown, &requested(CacheTtl::FiveMin));
}
#[test]
fn implicit_caching_does_not_imply_explicit_controls() {
    rejects(
        &CacheCapabilities::Known {
            implicit: true,
            explicit: None,
        },
        &requested(CacheTtl::ThirtyMin),
    );
}
#[test]
fn supported_explicit_lifetimes_pass_without_substitution() {
    let capabilities = explicit(vec![CacheTtl::FiveMin, CacheTtl::OneHour]);
    for ttl in [CacheTtl::FiveMin, CacheTtl::OneHour] {
        assert!(capabilities.validate(&requested(ttl)).is_ok());
    }
    rejects(&capabilities, &requested(CacheTtl::ThirtyMin));
}
#[test]
fn no_available_breakpoints_is_not_explicit_support() {
    rejects(
        &CacheCapabilities::Known {
            implicit: false,
            explicit: Some(ExplicitCacheCapabilities {
                max_points: 0,
                ttls: vec![CacheTtl::FiveMin],
            }),
        },
        &requested(CacheTtl::FiveMin),
    );
}
#[test]
fn off_and_auto_never_claim_to_disable_implicit_caching() {
    for capabilities in [
        CacheCapabilities::Unknown,
        CacheCapabilities::Known {
            implicit: true,
            explicit: None,
        },
        explicit(vec![CacheTtl::FiveMin]),
    ] {
        assert!(capabilities.validate(&CacheStrategy::Off).is_ok());
        assert!(capabilities.validate(&CacheStrategy::Auto).is_ok());
    }
}
#[test]
fn declared_provider_cap_is_inspectable_and_mapping_remains_documented() {
    let capabilities = explicit(vec![CacheTtl::FiveMin]);
    assert!(
        capabilities
            .validate(&CacheStrategy::Breakpoints {
                ttl: CacheTtl::FiveMin,
                points: vec![CacheTarget::System; 5]
            })
            .is_ok()
    );
    let CacheCapabilities::Known {
        explicit: Some(controls),
        ..
    } = capabilities
    else {
        panic!("explicit controls");
    };
    assert_eq!(controls.max_points, 4);
}
#[test]
fn request_validation_uses_exact_requested_ttl() {
    let mut request = ChatRequest::new("future-model");
    request.cache = requested(CacheTtl::ThirtyMin);
    let before = request.clone();
    assert!(
        request
            .validate_cache(&explicit(vec![CacheTtl::FiveMin]))
            .is_err()
    );
    assert_eq!(request, before);
}
#[test]
fn metadata_and_thirty_minute_ttl_round_trip() {
    let capabilities = explicit(vec![CacheTtl::ThirtyMin]);
    let encoded = serde_json::to_string(&capabilities).expect("serialize");
    assert_eq!(
        serde_json::from_str::<CacheCapabilities>(&encoded).expect("deserialize"),
        capabilities
    );
}

#[path = "provider_signed_replay/fixture.rs"]
mod fixture;

#[tokio::test]
async fn agent_rejects_undeclared_explicit_cache_before_transport() {
    let (mut agent, observed) = fixture::agent(
        vec![
            fixture::call(),
            fixture::end(stakit_ai_sdk::StopReason::ToolUse),
        ],
        fixture::Mutation::None,
    );
    agent = agent.cache(requested(CacheTtl::FiveMin));
    let outcome = agent.run().await.expect("outcome");
    assert!(matches!(
        outcome.finish,
        stakit_ai_sdk::Finish::Stopped { .. }
    ));
    assert!(observed.requests.lock().expect("requests").is_empty());
    assert_eq!(outcome.usage.input_tokens, 0);
}

#[path = "cache_capabilities/provider.rs"]
mod provider;

#[tokio::test]
async fn registered_providers_keep_independent_cache_declarations() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    for selected in ["explicit", "implicit"] {
        let explicit_calls = Arc::new(AtomicUsize::new(0));
        let implicit_calls = Arc::new(AtomicUsize::new(0));
        let mut agent = stakit_ai_sdk::Agent::new(())
            .provider(provider::CacheProvider {
                model: "explicit",
                controls: explicit(vec![CacheTtl::FiveMin]),
                calls: Arc::clone(&explicit_calls),
            })
            .register_provider(provider::CacheProvider {
                model: "implicit",
                controls: CacheCapabilities::Known {
                    implicit: true,
                    explicit: None,
                },
                calls: Arc::clone(&implicit_calls),
            })
            .model(selected)
            .cache(requested(CacheTtl::FiveMin));
        let outcome = agent.run().await.expect("outcome");
        if selected == "explicit" {
            assert!(matches!(outcome.finish, stakit_ai_sdk::Finish::EndTurn));
            assert_eq!(explicit_calls.load(Ordering::SeqCst), 1);
        } else {
            assert!(matches!(
                outcome.finish,
                stakit_ai_sdk::Finish::Stopped { .. }
            ));
            assert_eq!(explicit_calls.load(Ordering::SeqCst), 0);
        }
        assert_eq!(implicit_calls.load(Ordering::SeqCst), 0);
    }
}
