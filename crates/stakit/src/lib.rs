//! `stakit` — backend/API toolkit (facade crate).
//!
//! Re-exports the workspace crates behind a single dependency for easy DX.
//! Today that is [`model`]; `action` and `router` will follow.

#[doc(inline)]
pub use stakit_model as model;
