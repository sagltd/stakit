//! The action context.

use std::sync::Arc;

use serde_json::Value;

use crate::client::{ClientAction, ClientHandle};
use crate::{Action, Error};

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
        }
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
        let params = serde_json::to_value(&params).map_err(|e| Error::encode(&e))?;
        let value = self.client.call(C::NAME, params).await?;
        serde_json::from_value(value).map_err(|e| Error::decode(&e))
    }
}
