//! The cross-node [`Broker`] seam and its in-process implementation.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use futures::future::BoxFuture;
use hashbrown::HashSet;
use serde_json::Value;

use super::hub::BrokerSink;
use crate::Error;

/// Carries pub/sub traffic between nodes. One instance per node (per [`Hub`]).
///
/// The hub refcounts interest itself: it calls [`subscribe`](Broker::subscribe)
/// when a topic gets its first local subscriber and
/// [`unsubscribe`](Broker::unsubscribe) when the last one leaves, so a node only
/// receives topics it can deliver. Those two run under a hub lock — keep them
/// cheap and non-blocking (update a set; do I/O on a background task).
///
/// Payloads are opaque hub-encoded JSON. A broker never delivers a node's own
/// publishes back to it (the hub already delivered them locally).
///
/// [`Hub`]: crate::Hub
pub trait Broker: Send + Sync + 'static {
    /// Wires the owning hub in. Called once, by [`Hub::new`](crate::Hub::new).
    fn attach(&self, sink: BrokerSink);
    /// This node now has local subscribers for `topic`.
    fn subscribe(&self, topic: &str);
    /// This node no longer has local subscribers for `topic`.
    fn unsubscribe(&self, topic: &str);
    /// Checks that `payload` for `topic` fits this broker's limits. The hub
    /// calls it before delivering anything, so an oversized publish reaches
    /// nobody. Default: no limit.
    ///
    /// # Errors
    /// The limit error (e.g. `413`).
    fn validate(&self, _topic: &str, _payload: &[u8]) -> Result<(), Error> {
        Ok(())
    }
    /// Sends `payload` to every *other* node interested in `topic`. May return
    /// once the payload is queued; an error means it may not reach them.
    fn publish<'a>(&'a self, topic: &'a str, payload: Bytes) -> BoxFuture<'a, Result<(), Error>>;
    /// Asks the node holding device `topic` to run a client call
    /// ([`BrokerSink::call`]; nodes without it stay silent) and returns its
    /// answer, or `504` after `timeout`.
    fn call<'a>(
        &'a self,
        topic: &'a str,
        payload: Bytes,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, Error>>;
}

/// A node slot on an in-process bus.
#[derive(Default)]
struct Node {
    sink: Option<BrokerSink>,
    interest: HashSet<String>,
}

/// In-process [`Broker`]: the single-node default, and a way to link several
/// hubs in one process to simulate a cluster (tests).
///
/// ```ignore
/// let node_a = InMemoryBroker::new();
/// let node_b = node_a.link();          // same bus, second node
/// let (hub_a, hub_b) = (Hub::new(node_a), Hub::new(node_b));
/// ```
///
/// A `clone` is the *same* node (handy to keep for
/// [`has_interest`](Self::has_interest) after moving it into a hub);
/// [`link`](Self::link) adds a new one.
#[derive(Clone)]
pub struct InMemoryBroker {
    bus: Arc<Mutex<Vec<Node>>>,
    index: usize,
}

impl Default for InMemoryBroker {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryBroker {
    /// A fresh bus with this broker as its only node.
    #[must_use]
    pub fn new() -> Self {
        Self {
            bus: Arc::new(Mutex::new(vec![Node::default()])),
            index: 0,
        }
    }

    /// Adds another node to this broker's bus and returns its broker.
    #[must_use]
    pub fn link(&self) -> Self {
        let mut nodes = self.bus.lock().unwrap_or_else(PoisonError::into_inner);
        nodes.push(Node::default());
        Self {
            bus: Arc::clone(&self.bus),
            index: nodes.len() - 1,
        }
    }

    /// Whether this node currently listens for `topic` (it has local subscribers).
    #[must_use]
    pub fn has_interest(&self, topic: &str) -> bool {
        self.with_node(|node| node.interest.contains(topic))
    }

    fn with_node<T>(&self, f: impl FnOnce(&mut Node) -> T) -> T {
        let mut nodes = self.bus.lock().unwrap_or_else(PoisonError::into_inner);
        let mut detached = Node::default();
        f(nodes.get_mut(self.index).unwrap_or(&mut detached))
    }

    /// Sinks of the other nodes interested in `topic`.
    fn peers(&self, topic: &str) -> Vec<BrokerSink> {
        let nodes = self.bus.lock().unwrap_or_else(PoisonError::into_inner);
        nodes
            .iter()
            .enumerate()
            .filter(|(index, node)| *index != self.index && node.interest.contains(topic))
            .filter_map(|(_, node)| node.sink.clone())
            .collect()
    }
}

impl Broker for InMemoryBroker {
    fn attach(&self, sink: BrokerSink) {
        self.with_node(|node| node.sink = Some(sink));
    }

    fn subscribe(&self, topic: &str) {
        self.with_node(|node| node.interest.insert(topic.to_owned()));
    }

    fn unsubscribe(&self, topic: &str) {
        self.with_node(|node| node.interest.remove(topic));
    }

    fn publish<'a>(&'a self, topic: &'a str, payload: Bytes) -> BoxFuture<'a, Result<(), Error>> {
        for sink in self.peers(topic) {
            sink.deliver(topic, &payload);
        }
        Box::pin(async { Ok(()) })
    }

    fn call<'a>(
        &'a self,
        topic: &'a str,
        payload: Bytes,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, Error>> {
        Box::pin(async move {
            let answer = async {
                for sink in self.peers(topic) {
                    if let Some(result) = sink.call(topic, &payload).await {
                        return result;
                    }
                }
                Err(no_subscriber())
            };
            tokio::time::timeout(timeout, answer)
                .await
                .unwrap_or_else(|_| Err(Error::coded(504, "timeout", "client action timed out")))
        })
    }
}

/// No connection anywhere is subscribed to the target topic.
fn no_subscriber() -> Error {
    Error::coded(404, "not_found", "no connection subscribed to the topic")
}
