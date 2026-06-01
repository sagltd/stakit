//! The [`Router`] and its builder.

use std::sync::Arc;

use futures::{Stream, StreamExt as _};
use hashbrown::HashMap;
use serde_json::{Map, Value};

use stakit_model::TSType;

use crate::action::{ErasedAction, ErasedStreamAction};
use crate::client::{ClientAction, ClientHandle, ClientMeta};
use crate::session::Session;
use crate::{Action, Cx, Error, Frame, Reply, StreamAction};

/// A registry of actions, generic over the global ctx `G` and request ctx `R`.
///
/// Transport-agnostic and **payload-routed**: the action name lives *inside* the
/// request payload, never in the URL. One handler per transport feeds the whole
/// payload to the router, which dispatches every call to the right action. Feed
/// it via [`Router::on_request`] (unary), [`Router::on_stream`] (streaming), or
/// [`Router::session`] (duplex/websocket). Wire it into any framework yourself.
///
/// A payload is one of:
/// - object — `{ "greet": {…}, "count": {…} }` (keyed; response is an object)
/// - array — `[["greet", {…}], ["greet", {…}]]` (ordered, duplicates allowed;
///   response is an array in the same order)
pub struct Router<G, R> {
    pub(crate) app: Arc<G>,
    pub(crate) actions: HashMap<&'static str, Arc<dyn ErasedAction<G, R>>>,
    pub(crate) streams: HashMap<&'static str, Arc<dyn ErasedStreamAction<G, R>>>,
    pub(crate) client_actions: Vec<ClientMeta>,
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
            client_actions: Vec::new(),
        }
    }

    /// Dispatches a unary request. The `payload` carries every call (`action ->
    /// params`); each is deserialized, validated, run, and the results are
    /// assembled into a response value mirroring the payload's shape (object in →
    /// object out, array in → array out). Independent calls run concurrently; the
    /// request itself never fails (per-call errors ride in each [`Reply`]).
    pub async fn on_request(&self, req: R, payload: Value) -> Value
    where
        R: Clone,
    {
        match payload {
            Value::Object(map) => {
                let mut out = Map::with_capacity(map.len());
                // Single-call fast path: dispatch in place, no intermediate Vec.
                if map.len() <= 1 {
                    if let Some((action, params)) = map.into_iter().next() {
                        let reply = self.dispatch_one(req, &action, params).await;
                        out.insert(action, reply.into_value());
                    }
                } else {
                    for (action, reply) in self.run_all(req, map.into_iter().collect()).await {
                        out.insert(action, reply.into_value());
                    }
                }
                Value::Object(out)
            }
            Value::Array(items) => {
                let replies = self.run_all(req, parse_array(items)).await;
                Value::Array(replies.into_iter().map(|(_, r)| r.into_value()).collect())
            }
            _ => Value::Object(Map::new()),
        }
    }

    /// Runs several `(action, params)` entries concurrently, preserving order.
    async fn run_all(&self, req: R, entries: Vec<(String, Value)>) -> Vec<(String, Reply)>
    where
        R: Clone,
    {
        let futures = entries.into_iter().map(|(action, params)| {
            let req = req.clone();
            async move {
                let reply = self.dispatch_one(req, &action, params).await;
                (action, reply)
            }
        });
        futures::future::join_all(futures).await
    }

    /// Routes one call to its action (404 if unknown), runs it, wraps the result.
    async fn dispatch_one(&self, req: R, action: &str, params: Value) -> Reply {
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

    /// Dispatches a streaming request. The `payload` carries one or more stream
    /// calls; their [`Frame`]s are merged (each tagged with its `index` +
    /// `action`) into one `'static` stream that drops straight into axum's
    /// `Body::from_stream(...)` / a websocket sink. Each call ends with its own
    /// `End` frame (or an `Error` that terminates only that call).
    pub fn on_stream(&self, req: R, payload: Value) -> impl Stream<Item = Frame> + use<G, R>
    where
        R: Clone,
    {
        let app = Arc::clone(&self.app);
        let entries = match payload {
            Value::Array(items) => parse_array(items),
            Value::Object(map) => map.into_iter().collect(),
            _ => Vec::new(),
        };
        // Resolve handlers now (while we still hold `&self`): only the matched
        // `Arc`s are cloned, not the whole stream registry.
        let resolved: Vec<_> = entries
            .into_iter()
            .enumerate()
            .map(|(index, (action, params))| {
                let handler = self.streams.get(action.as_str()).cloned();
                (index, action, params, handler)
            })
            .collect();
        async_stream::stream! {
            let mut subs = Vec::with_capacity(resolved.len());
            for (index, action, params, handler) in resolved {
                subs.push(Box::pin(action_stream(
                    index,
                    action,
                    params,
                    handler,
                    Arc::clone(&app),
                    req.clone(),
                )));
            }
            let mut merged = futures::stream::select_all(subs);
            while let Some(frame) = merged.next().await {
                yield frame;
            }
        }
    }

    /// Generates a TypeScript client definition for every registered action.
    #[must_use]
    pub fn generate_ts(&self) -> String {
        crate::ts::generate(&self.actions, &self.streams, &self.client_actions)
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

/// One action's contribution to a streaming response, as tagged [`Frame`]s.
fn action_stream<G, R>(
    index: usize,
    action: String,
    params: Value,
    handler: Option<Arc<dyn ErasedStreamAction<G, R>>>,
    app: Arc<G>,
    req: R,
) -> impl Stream<Item = Frame>
where
    G: Send + Sync + 'static,
    R: Send + Sync + 'static,
{
    async_stream::stream! {
        let Some(handler) = handler else {
            yield Frame::error(index, action.clone(), Error::not_found(&action));
            return;
        };
        let cx = Cx {
            app,
            req,
            client: ClientHandle::default(),
        };
        let dispatched = handler.dispatch(&cx, params);
        match dispatched {
            Err(error) => yield Frame::error(index, action, error),
            Ok(mut items) => {
                while let Some(item) = items.next().await {
                    match item {
                        Ok(value) => yield Frame::next(index, action.clone(), value),
                        Err(error) => {
                            yield Frame::error(index, action, error);
                            return;
                        }
                    }
                }
                yield Frame::end(index, action);
            }
        }
    }
}

/// Parses an array payload `[["action", params], …]` into ordered entries,
/// skipping any malformed element.
fn parse_array(items: Vec<Value>) -> Vec<(String, Value)> {
    items
        .into_iter()
        .filter_map(|element| {
            let Value::Array(pair) = element else {
                return None;
            };
            let mut pair = pair.into_iter();
            let action = pair.next()?.as_str()?.to_owned();
            let params = pair.next().unwrap_or(Value::Null);
            Some((action, params))
        })
        .collect()
}

/// Builder for [`Router`].
pub struct Builder<G, R> {
    app: Option<Arc<G>>,
    actions: HashMap<&'static str, Arc<dyn ErasedAction<G, R>>>,
    streams: HashMap<&'static str, Arc<dyn ErasedStreamAction<G, R>>>,
    client_actions: Vec<ClientMeta>,
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

    /// Declares a client action (server→client). Used by `cx.client_call::<C>()`
    /// and included in the generated TypeScript.
    #[must_use]
    pub fn client_action<C: ClientAction>(mut self) -> Self {
        self.client_actions.push(ClientMeta {
            name: C::NAME,
            params_ts: <C::Params as TSType>::to_ts(),
            return_ts: <C::Return as TSType>::to_ts(),
        });
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
            client_actions: self.client_actions,
        }
    }
}
