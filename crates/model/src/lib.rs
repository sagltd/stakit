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

mod model;
mod ts_type;
pub mod validate;

pub use model::{Model, generate_typescript};
pub use ts_type::TSType;
pub use validate::{Validate, ValidationError, ValidationErrors};

/// Common imports. `use stakit_model::prelude::*;` brings the traits (so
/// `.validate()` / `.to_ts()` resolve), the derive, and the error types.
pub mod prelude {
    pub use crate::{
        Model, TSType, Validate, ValidationError, ValidationErrors, generate_typescript,
    };
}

// The derive macro shares the name `Model` with the trait above (macro vs type
// namespace), so `use stakit_model::Model;` enables both `#[derive(Model)]` and
// the `T: Model` bound — same ergonomics as serde's `Serialize`.
pub use stakit_model_derive::Model;
