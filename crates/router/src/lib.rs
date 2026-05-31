//! `stakit-router` — framework- and format-agnostic action router.
//!
//! Validates input (via `stakit-model`), routes to the right action, supports
//! typed action-to-action calls, and generates a TypeScript client. It knows
//! nothing about HTTP/WebSockets or JSON specifically — you wire it into your
//! framework (axum, hyper, …) and hand it already-decoded params + a request
//! context. See `docs/router.md`.

mod action;
mod client;
mod cx;
mod error;
mod reply;
mod router;
mod session;
mod ts;

pub use action::{Action, StreamAction};
pub use client::ClientAction;
pub use cx::Cx;
pub use error::Error;
pub use reply::{ErrorBody, Frame, Reply};
pub use router::{Builder, Router};
pub use session::Session;

/// Re-exports so `#[action]`-generated code can name the boxed future/stream types.
#[doc(hidden)]
pub use futures::future::BoxFuture;
#[doc(hidden)]
pub use futures::stream::BoxStream;

/// The `#[action]` attribute macro.
pub use stakit_router_derive::action;
