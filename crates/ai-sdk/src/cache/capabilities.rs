//! Provider-neutral metadata for the cache controls an adapter implements.
use crate::{CacheStrategy, CacheTtl, ProviderError};
use serde::{Deserialize, Serialize};

/// Cache behavior implemented by a provider adapter for a particular model.
///
/// This describes available controls, not a promise that any request gets a
/// cache hit. Implicit retention and eligibility remain provider-managed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CacheCapabilities {
    /// The adapter has not declared its cache controls. Automatic/default
    /// requests remain supported; explicit caching needs a declaration.
    #[default]
    Unknown,
    /// The adapter's implemented implicit and explicit cache controls.
    Known {
        /// Whether eligible prompts may be cached without explicit markers.
        implicit: bool,
        /// Explicit breakpoint controls, when implemented by the adapter.
        explicit: Option<ExplicitCacheCapabilities>,
    },
}

/// Explicit breakpoint caching implemented by an adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplicitCacheCapabilities {
    /// Maximum breakpoints written in one request. Strategies with more targets
    /// may be capped in their documented provider order; this is no hit guarantee.
    pub max_points: usize,
    /// Lifetimes accepted by the adapter without silently substituting another.
    pub ttls: Vec<CacheTtl>,
}

impl CacheCapabilities {
    /// Checks whether a requested strategy can be honored by this adapter.
    ///
    /// `Off` disables explicit markers only. `Auto` uses the best available
    /// behavior, including none; neither promises to disable implicit caching.
    ///
    /// # Errors
    /// Returns [`ProviderError::InvalidArgument`] for explicit breakpoints when
    /// support is undeclared, absent, or does not include the requested TTL.
    pub fn validate(&self, strategy: &CacheStrategy) -> Result<(), ProviderError> {
        let CacheStrategy::Breakpoints { ttl, .. } = strategy else {
            return Ok(());
        };
        let Self::Known { explicit, .. } = self else {
            return Err(ProviderError::InvalidArgument(
                "provider has not declared explicit prompt-cache support".into(),
            ));
        };
        let Some(controls) = explicit else {
            return Err(ProviderError::InvalidArgument(
                "provider adapter does not support explicit prompt-cache breakpoints".into(),
            ));
        };
        if controls.max_points == 0 || !controls.ttls.contains(ttl) {
            return Err(ProviderError::InvalidArgument(
                "provider adapter does not support the requested prompt-cache lifetime or breakpoints".into(),
            ));
        }
        Ok(())
    }
}
