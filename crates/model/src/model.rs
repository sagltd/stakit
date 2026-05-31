//! The [`Model`] trait and the [`generate_typescript`] entrypoint.

use crate::{TSType, Validate};

/// A validatable, TypeScript-exportable type.
///
/// Blanket-implemented for everything that is both [`Validate`] and [`TSType`]
/// — which `#[derive(Model)]` provides. Use it as a single bound (`T: Model`)
/// when you need both capabilities.
pub trait Model: Validate + TSType {}

impl<T: Validate + TSType> Model for T {}

/// Generates the TypeScript definition for model `M`.
///
/// Equivalent to `M::to_ts()`; provided as a discoverable entrypoint and the
/// future home of transitive multi-type emission.
#[must_use]
pub fn generate_typescript<M: TSType>() -> String {
    M::to_ts()
}
