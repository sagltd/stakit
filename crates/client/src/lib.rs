//! `stakit-client` — typed Rust client for stakit routers.
//!
//! One cheap, cloneable [`Client`] handle talks to any number of servers over
//! HTTP (unary + stream) and websocket (duplex). The action's generated unit
//! struct is its [`Endpoint`](stakit_router::Endpoint), so a single token drives
//! param/result inference:
//!
//! ```ignore
//! use stakit_client::{Client, CallOpts};
//!
//! let client = Client::builder("https://main")
//!     .header("authorization", token)
//!     .build();
//!
//! // base url + headers
//! let res = client.fetch(greet, Greet { name: "a".into() }).await?;
//! if let Some(g) = res.data() { /* typed Greeting */ }
//!
//! // fan out to another server, request-only override (base untouched)
//! let res = client
//!     .fetch_with(greet, params, CallOpts::new().url("https://vm-42").header("authorization", vm))
//!     .await?;
//! ```
//!
//! A network failure is an `Err(TransportError)`; an application error is
//! `Ok(ActionResult::Error(..))` — never a panic. See `docs/transport.md` for
//! the wire contract shared with the TypeScript client.

mod client;
mod error;
mod options;
mod pool;
mod result;
mod stream;
mod ws;

pub use client::{Batch, BatchResults, Builder, Client};
pub use error::TransportError;
pub use options::{CallOpts, Method};
pub use result::ActionResult;
pub use ws::{Connection, ServerFrame};

/// Re-exported so callers can name error bodies and endpoint descriptors without
/// depending on `stakit-router` directly.
pub use stakit_router::{Endpoint, ErrorBody, Kind};
