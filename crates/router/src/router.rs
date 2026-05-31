//! The [`Router`] and its builder.

use std::collections::HashMap;
use std::sync::Arc;

use futures::{Stream, StreamExt as _};
use serde_json::Value;

use crate::action::{ErasedAction, ErasedStreamAction};
use crate::client::ClientHandle;
use crate::session::Session;
use crate::{Action, Cx, Error, Frame, Reply, StreamAction};

/// A registry of actions, generic over the global ctx `G` and request ctx `R`.
///
/// Transport-agnostic: feed it already-decoded params + a request ctx via
/// [`Router::on_request`] (unary), [`Router::on_stream`] (streaming), or
/// [`Router::session`] (duplex/websocket). Wire it into any framework yourself.
pub struct Router<G, R> {
    pub(crate) app: Arc<G>,
    pub(crate) actions: HashMap<&'static str, Arc<dyn ErasedAction<G, R>>>,
    pub(crate) streams: HashMap<&'static str, Arc<dyn ErasedStreamAction<G, R>>>,
}

impl<G, R> Router<G, R>
where
    G: Send + Sync + 'static,
    R: Send + Sync + 'static,
{
    /// Starts building a router.
    #[must_use]
    pub fn builder() -> Builder<G, R> {
        Builder {
            app: None,
            actions: HashMap::new(),
            streams: HashMap::new(),
        }
    }

    /// Dispatches a unary request: routes by `action`, deserializes + validates
    /// `params`, runs, and returns a serializable [`Reply`].
    pub async fn on_request(&self, req: R, action: &str, params: Value) -> Reply {
        let Some(handler) = self.actions.get(action) else {
            return Reply::error(Error::not_found(action));
        };
        let cx = Cx {
            app: Arc::clone(&self.app),
            req,
            client: ClientHandle::default(),
        };
        match handler.dispatch(&cx, params).await {
            Ok(data) => Reply::ok(data),
            Err(error) => Reply::error(error),
        }
    }

    /// Dispatches a streaming request, yielding [`Frame`]s (`Next*`, then `End`,
    /// or an `Error`). The returned stream is `'static` so it drops straight into
    /// axum's `Sse::new(...)` or a websocket sink.
    pub fn on_stream(
        &self,
        req: R,
        action: &str,
        params: Value,
    ) -> impl Stream<Item = Frame> + use<G, R> {
        let app = Arc::clone(&self.app);
        let handler = self.streams.get(action).cloned();
        let name = action.to_owned();
        async_stream::stream! {
            let Some(handler) = handler else {
                yield Frame::error(Error::not_found(&name));
                return;
            };
            let cx = Cx { app, req, client: ClientHandle::default() };
            match handler.dispatch(&cx, params) {
                Err(error) => yield Frame::error(error),
                Ok(mut stream) => {
                    while let Some(item) = stream.next().await {
                        match item {
                            Ok(value) => yield Frame::next(value),
                            Err(error) => {
                                yield Frame::error(error);
                                return;
                            }
                        }
                    }
                }
            }
            yield Frame::End;
        }
    }

    /// Generates a TypeScript client definition for every registered action.
    #[must_use]
    pub fn generate_ts(&self) -> String {
        crate::ts::generate(&self.actions, &self.streams)
    }
}

impl<G, R> Router<G, R>
where
    G: Send + Sync + 'static,
    R: Clone + Send + Sync + 'static,
{
    /// Opens a duplex session for a websocket connection (requires a Tokio
    /// runtime). Pump inbound frames through [`Session::handle`] and forward
    /// [`Session::outgoing`] to the socket; actions can `cx.client_call(...)`.
    #[must_use]
    pub fn session(self: &Arc<Self>, req: R) -> Session<G, R> {
        Session::new(Arc::clone(self), req)
    }
}

/// Builder for [`Router`].
pub struct Builder<G, R> {
    app: Option<Arc<G>>,
    actions: HashMap<&'static str, Arc<dyn ErasedAction<G, R>>>,
    streams: HashMap<&'static str, Arc<dyn ErasedStreamAction<G, R>>>,
}

impl<G, R> Builder<G, R>
where
    G: Send + Sync + 'static,
    R: Send + Sync + 'static,
{
    /// Sets the global context (pass `()` if you have none).
    #[must_use]
    pub fn ctx(mut self, app: G) -> Self {
        self.app = Some(Arc::new(app));
        self
    }

    /// Registers a unary action (keyed by its `name()`).
    #[must_use]
    pub fn register<A: Action<G, R>>(mut self, action: A) -> Self {
        self.actions.insert(Action::name(&action), Arc::new(action));
        self
    }

    /// Registers a streaming action (keyed by its `name()`).
    #[must_use]
    pub fn register_stream<S: StreamAction<G, R>>(mut self, action: S) -> Self {
        self.streams
            .insert(StreamAction::name(&action), Arc::new(action));
        self
    }

    /// Finalizes the router.
    ///
    /// # Panics
    /// Panics if [`Builder::ctx`] was never called.
    #[must_use]
    pub fn build(self) -> Router<G, R> {
        Router {
            app: self
                .app
                .expect("Router::builder().ctx(...) must be set before build()"),
            actions: self.actions,
            streams: self.streams,
        }
    }
}
