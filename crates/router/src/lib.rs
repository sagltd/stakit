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
mod endpoint;
mod error;
mod middleware;
mod reply;
mod router;
mod session;
mod ts;

pub use action::{Action, StreamAction};
pub use client::{ClientAction, DEFAULT_CLIENT_CALL_TIMEOUT};
pub use cx::Cx;
pub use endpoint::{Endpoint, Kind};
pub use error::{Error, ErrorCodes, ResponseError};
pub use middleware::{ActionExt, Guarded, Middleware, StreamActionExt, StreamGuarded};
pub use reply::{ErrorBody, Frame, Reply};
pub use router::{Builder, Router};
pub use session::Session;

// Convenience re-exports from `stakit-model` so a single `stakit-router`
// dependency gives you the validation + TypeScript traits/types too. Note: the
// `#[derive(Model)]` / `#[action]` macros expand to `::stakit_model` /
// `::stakit_router` paths, so a crate that *uses the macros* must still depend on
// `stakit-model` directly (see `examples/axum-server`).
#[doc(inline)]
pub use stakit_model as model;
pub use stakit_model::{
    Model, TSType, Validate, ValidationError, ValidationErrors, generate_typescript,
};
// The `#[model]` attribute macro (and `Model` derive, via the line above), so a
// `stakit-router` import is enough to declare models. Coexists with the `model`
// module alias (macro vs module namespace).
pub use stakit_model::model;

/// Re-export so `#[action(stream)]`-generated code can name the `Stream` trait
/// without the user crate needing a direct `futures` dependency.
#[doc(hidden)]
pub use futures::Stream;

/// The `#[action]` attribute macro.
pub use stakit_router_derive::action;

/// The `#[derive(ResponseError)]` macro: declare an action error's HTTP status,
/// machine code, and client message via attributes.
pub use stakit_router_derive::ResponseError;
