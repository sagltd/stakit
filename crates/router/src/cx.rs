//! The action context.

use std::future::Future;
use std::sync::Arc;

use serde_json::Value;

use crate::client::{ClientAction, ClientHandle};
use crate::{Action, Channel, Error, Hub};

/// Context handed to every action.
///
/// - `G` — **app / global state**: built once at `Router::build` and shared
///   (behind `Arc`) across every request. Your **database connection pool**,
///   config, HTTP clients, caches.
/// - `R` — **per-request context**: built fresh for each request. The current
///   user/auth, request headers, uploaded files, a request id — whatever that
///   request needs. You construct it in your framework handler.
pub struct Cx<G, R> {
    /// App / global state — shared across all requests (e.g. a db pool).
    pub app: Arc<G>,
    /// Per-request context — built fresh per request (e.g. the current user).
    pub req: R,
    pub(crate) client: ClientHandle,
    pub(crate) hub: Hub,
}

impl<G, R> Cx<G, R> {
    /// Builds a context for **unit-testing an action directly**, with no server
    /// or transport — call the action's `run` (or [`Cx::call`]) and assert on the
    /// typed result:
    ///
    /// ```ignore
    /// let cx = Cx::test(App { .. }, Auth { admin: true });
    /// let out = greet.run(&cx, Greet { name: "bob".into() }).await?;
    /// assert_eq!(out.message, "Hello, bob!");
    /// ```
    ///
    /// `client_call` errors here (no client); chain [`Cx::with_client`] to stub it.
    #[must_use]
    pub fn test(app: G, req: R) -> Self {
        Self {
            app: Arc::new(app),
            req,
            client: ClientHandle::default(),
            hub: Hub::default(),
        }
    }

    /// Uses `hub` for [`Cx::publish`] / [`Cx::client_call_device`] — chain it onto
    /// [`Cx::test`] to observe publishes in a unit test.
    #[must_use]
    pub fn with_hub(mut self, hub: Hub) -> Self {
        self.hub = hub;
        self
    }

    /// The pub/sub hub of the router running this action.
    #[must_use]
    pub const fn hub(&self) -> &Hub {
        &self.hub
    }

    /// Stubs `client_call` with an in-process handler — chain it onto
    /// [`Cx::test`]. The handler gets the action name + JSON params and returns
    /// the JSON reply (use `serde_json::to_value` / `json!` for typed returns):
    ///
    /// ```ignore
    /// let cx = Cx::test(App { .. }, req).with_client(|name, _params| {
    ///     assert_eq!(name, "showToast");
    ///     Ok(serde_json::json!("delivered"))
    /// });
    /// let out = notify.run(&cx, Greet { name: "x".into() }).await?;
    /// ```
    #[must_use]
    pub fn with_client(
        mut self,
        handler: impl Fn(&str, Value) -> Result<Value, Error> + Send + Sync + 'static,
    ) -> Self {
        self.client = ClientHandle::mock(Arc::new(handler));
        self
    }

    /// Calls another action in-process with typed params (no (de)serialization).
    ///
    /// Returns the callee's own error type; compose it into your action's error
    /// with `?`. Typed input is trusted — the wire entry points validate.
    ///
    /// # Errors
    /// Propagates the called action's error.
    pub async fn call<A>(&self, action: A, input: A::Input) -> Result<A::Output, A::Error>
    where
        A: Action<G, R>,
    {
        action.run(self, input).await
    }

    /// Invokes a **client** action over a duplex connection and awaits its reply.
    ///
    /// # Errors
    /// Errors if there is no duplex connection (unary/stream transports), if the
    /// connection closes, or if the client's reply fails to deserialize.
    pub async fn client_call<C>(&self, params: C::Params) -> Result<C::Return, Error>
    where
        C: ClientAction,
    {
        typed_call::<C, _, _>(params, |params| self.client.call(C::NAME, params)).await
    }

    /// Publishes client action `C` to every connection subscribed to
    /// `channel`, on every node (fire-and-forget, at-most-once). Works from any
    /// transport — an HTTP action can notify websocket subscribers.
    ///
    /// # Errors
    /// A bad channel key, unserializable params, or a broker rejection (e.g.
    /// payload too large for the broker).
    pub async fn publish<C>(&self, channel: &impl Channel, params: &C::Params) -> Result<(), Error>
    where
        C: ClientAction,
    {
        self.hub.publish::<C>(&channel.topic()?, params).await
    }

    /// Invokes client action `C` on the connection holding `user`'s `device`
    /// — on this node or any other — and awaits its reply. The device must be
    /// bound to `user` (see [`Identity`](crate::Identity)), so an action can't
    /// be tricked into calling another user's device.
    ///
    /// # Errors
    /// - `404` when no connection holds the device and the broker knows it
    ///   (this node / [`InMemoryBroker`](crate::InMemoryBroker));
    /// - `504` when the broker can't know (e.g. Postgres: nodes without the
    ///   device stay silent) or the device doesn't answer in time;
    /// - the device's own error (clamped to 4xx/5xx), or a reply that fails to
    ///   deserialize.
    pub async fn client_call_device<C>(
        &self,
        user: &str,
        device: &str,
        params: C::Params,
    ) -> Result<C::Return, Error>
    where
        C: ClientAction,
    {
        typed_call::<C, _, _>(params, |params| {
            self.hub.client_call_device(user, device, C::NAME, params)
        })
        .await
    }
}

/// Encodes typed params, runs the JSON-level `call`, decodes the typed reply.
pub(crate) async fn typed_call<C, F, Fut>(params: C::Params, call: F) -> Result<C::Return, Error>
where
    C: ClientAction,
    F: FnOnce(Value) -> Fut,
    Fut: Future<Output = Result<Value, Error>>,
{
    let params = serde_json::to_value(&params).map_err(|e| Error::encode(&e))?;
    serde_json::from_value(call(params).await?).map_err(|e| Error::decode(&e))
}
