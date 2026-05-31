//! `stakit` — backend/API toolkit facade.
//!
//! Re-exports the workspace crates behind one dependency:
//! - [`mod@model`] — validation + TypeScript types ([`Model`], [`Validate`], …).
//! - [`mod@router`] — actions, routing, duplex sessions ([`Router`], [`action`], …).
//!
//! ```ignore
//! use stakit::{Model, Validate, action, Cx, Error, Router};
//! ```
//!
//! Note: because the `#[derive(Model)]` and `#[action]` macros expand to
//! `::stakit_model` / `::stakit_router` paths, crates that use the macros should
//! also depend on `stakit-model` and `stakit-router` directly (see the
//! `examples/axum-server` demo). The flat re-exports below are for the traits,
//! types, and helpers.

#[doc(inline)]
pub use stakit_model as model;
#[doc(inline)]
pub use stakit_router as router;

// --- flat conveniences ---
pub use stakit_model::{
    Model, TSType, Validate, ValidationError, ValidationErrors, generate_typescript,
};
pub use stakit_router::{
    Action, ClientAction, Cx, Error, ErrorBody, Frame, Reply, Router, Session, StreamAction,
    action, err,
};
