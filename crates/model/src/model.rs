//! The [`Model`] trait and the [`generate_typescript`] entrypoint.

use crate::{ModelError, TSType};

/// A type that can be validated and exported to TypeScript.
///
/// Blanket-implemented for everything that is both [`garde::Validate`] (with the
/// unit context) and [`TSType`] — which `#[derive(Model)]` provides.
pub trait Model: garde::Validate<Context = ()> + TSType {
    /// Validates `self`, mapping any failure to [`ModelError`].
    ///
    /// # Errors
    /// Returns [`ModelError::Invalid`] if any `#[garde(...)]` rule fails; the
    /// error aggregates every failing field.
    fn validate_model(&self) -> Result<(), ModelError>;
}

impl<T> Model for T
where
    T: garde::Validate<Context = ()> + TSType,
{
    fn validate_model(&self) -> Result<(), ModelError> {
        self.validate().map_err(ModelError::from)
    }
}

/// Generates the TypeScript definition for model `M`.
///
/// Equivalent to `M::to_ts()`; provided as a discoverable entrypoint and the
/// future home of transitive multi-type emission.
#[must_use]
pub fn generate_typescript<M: TSType>() -> String {
    M::to_ts()
}
