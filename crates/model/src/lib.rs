//! `stakit-model` — derive-based validation and TypeScript type export.
//!
//! Annotate a struct or enum with `#[derive(Model)]` and `#[validate(...)]`
//! rules to get:
//! - fast, allocation-free-on-success validation via the [`Validate`] trait,
//!   surfaced as [`ValidationErrors`];
//! - a TypeScript `interface`/union via the [`TSType`] trait / [`generate_typescript`].
//!
//! The validation rule functions live in [`mod@validate`] and are reusable on
//! their own. Design notes: `docs/architecture.md`. Inspired by `ggtype`.

#[cfg(feature = "schema")]
mod json_schema;
#[path = "model.rs"]
mod model_trait;
mod ts_type;
pub mod validate;

#[cfg(feature = "schema")]
pub use json_schema::JsonSchema;
pub use model_trait::{Model, generate_typescript};
pub use ts_type::TSType;
pub use validate::{Validate, ValidationError, ValidationErrors};

/// Common imports. `use stakit_model::prelude::*;` brings the traits (so
/// `.validate()` / `.to_ts()` resolve), the derives, and the error types.
pub mod prelude {
    #[cfg(feature = "schema")]
    pub use crate::JsonSchema;
    pub use crate::{
        Model, TSType, Validate, ValidationError, ValidationErrors, generate_typescript, model,
    };
}

// The `Model` derive shares its name with the `Model` trait above (macro vs type
// namespace), like serde's `Serialize`. `#[model]` is the one-annotation form
// that also wires up serde (+ camelCase under the `camel` feature).
pub use stakit_model_derive::{Model, model};

// The `JsonSchema` derive shares its name with the trait above, like `Model`.
#[cfg(feature = "schema")]
pub use stakit_model_derive::JsonSchema;

/// Re-export so `#[derive(JsonSchema)]`-generated code can reference
/// `serde_json` without the downstream crate depending on it directly.
#[cfg(feature = "schema")]
#[doc(hidden)]
pub use serde_json as __serde_json;
