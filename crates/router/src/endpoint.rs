//! Typed endpoint descriptors for the Rust client.
//!
//! `#[action]` emits an [`Endpoint`] impl on the action's generated unit struct,
//! so a remote client can recover the action's name, kind, param, and result
//! types from that one token: `client.fetch(greet, params)`.

/// Whether an endpoint is a unary call or a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A single request → single reply.
    Unary,
    /// A single request → a stream of items.
    Stream,
}

/// A typed handle to one registered action, carried by its generated unit
/// struct. Lets the Rust client infer params + output from a single token.
///
/// Emitted by `#[action]`; you rarely implement it by hand. The
/// `Serialize`/`DeserializeOwned` bounds the client needs are required at the
/// call site, not here, so every action gets an `Endpoint` impl regardless of
/// whether its types are round-trippable.
pub trait Endpoint {
    /// The action's validated input type (`()` when param-less).
    type Params;
    /// The action's output type (unary) or stream item type.
    type Output;
    /// The stable action name (matches the router registration key).
    const ACTION: &'static str;
    /// Whether this is a unary or streaming endpoint.
    const KIND: Kind;
}
