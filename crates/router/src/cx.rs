//! The action context.

use std::sync::Arc;

use crate::client::{ClientAction, ClientHandle};
use crate::{Action, Error};

/// Context handed to every action.
///
/// `G` is the **global** context (built once at router build — db pools, config,
/// …). `R` is the **request** context (per request — auth, uploaded images,
/// headers, anything; you build it in your framework handler).
pub struct Cx<G, R> {
    /// Shared global context.
    pub app: Arc<G>,
    /// Per-request context.
    pub req: R,
    pub(crate) client: ClientHandle,
}

impl<G, R> Cx<G, R> {
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
