//! `stakit-model` — derive-based validation and TypeScript type export.
//!
//! Annotate a struct with `#[derive(Model)]` and native `#[garde(...)]` rules to get:
//! - runtime validation via [`garde`](https://docs.rs/garde), surfaced as [`ModelError`];
//! - a TypeScript `interface` via the [`TSType`] trait / [`generate_typescript`].
//!
//! Design notes live in `docs/architecture.md`. Inspired by the `ggtype` TS library.

mod error;
mod model;
mod ts_type;

pub use error::ModelError;
pub use model::{Model, generate_typescript};
pub use ts_type::TSType;

/// Re-export of `garde` so downstream crates need only depend on `stakit-model`.
#[doc(hidden)]
pub use garde;

/// Implementation details referenced by `#[derive(Model)]`-generated code.
///
/// Not part of the public API; may change without notice.
#[doc(hidden)]
pub mod __private {
    pub use garde;
    pub use garde::util::nested_path;
}

// The derive macro shares the name `Model` with the trait above (macro vs type
// namespace), so `use stakit_model::Model;` enables both `#[derive(Model)]` and
// the `T: Model` bound — same ergonomics as serde's `Serialize`.
pub use stakit_model_derive::Model;
