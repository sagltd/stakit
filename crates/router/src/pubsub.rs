//! Pub/sub: typed channels, the per-node [`Hub`], and the cross-node [`Broker`].
//!
//! See `docs/pubsub.md` for the model, guarantees and limits.

pub(crate) mod broker;
pub(crate) mod channel;
pub(crate) mod hub;
