//! `stakit` — backend/API toolkit (facade crate).
//!
//! Re-exports the workspace crates behind a single dependency for easy DX:
//! [`model`] (validation + TS types) and [`router`] (actions + routing).

#[doc(inline)]
pub use stakit_model as model;
#[doc(inline)]
pub use stakit_router as router;
