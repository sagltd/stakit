//! The [`Router`] and its builder.

use std::sync::Arc;
use std::time::Duration;

use futures::{Stream, StreamExt as _};
use hashbrown::HashMap;
use serde_json::value::RawValue;
use serde_json::{Map, Value};

use stakit_model::TSType;

use crate::action::{ErasedAction, ErasedBorrowAction, ErasedStreamAction};
use crate::client::{ClientAction, ClientHandle, ClientMeta, DEFAULT_CLIENT_CALL_TIMEOUT};
use crate::session::Session;
use crate::{Action, BorrowAction, Cx, Error, Frame, Reply, StreamAction};

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
    pub(crate) borrow_actions: HashMap<&'static str, Arc<dyn ErasedBorrowAction<G, R>>>,
    pub(crate) streams: HashMap<&'static str, Arc<dyn ErasedStreamAction<G, R>>>,
    pub(crate) client_actions: Vec<ClientMeta>,
    pub(crate) client_call_timeout: Duration,
    pub(crate) on_error: ErrorHook,
}

/// A hook that transforms every outgoing error before it reaches the client —
/// e.g. redact details in production, or surface them in development.
pub(crate) type ErrorHook = Arc<dyn Fn(Error) -> Error + Send + Sync>;

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
            borrow_actions: HashMap::new(),
            streams: HashMap::new(),
            client_actions: Vec::new(),
            client_call_timeout: DEFAULT_CLIENT_CALL_TIMEOUT,
            on_error: Arc::new(|error| error),
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

    /// **Zero-copy** unary dispatch: feed the raw request body bytes and the
    /// router parses the envelope *and* each call's params straight out of that
    /// buffer — borrow actions ([`Builder::register_borrow`]) get `&'a str` /
    /// `Cow` fields pointing into `body` with no intermediate [`Value`] and no
    /// per-field copy.
    ///
    /// A strict superset of [`Router::on_request`] for the object/array payload
    /// shapes: any call naming a plain [`Action`] falls back to the owned path
    /// (one parse into a [`Value`]), so a mixed router works through this one
    /// entrypoint. `body` must outlive the call (it does — it's borrowed here).
    pub async fn on_request_borrowed(&self, req: R, body: &[u8]) -> Value
    where
        R: Clone,
    {
        // Object form: `{ "action": params, … }` — keys + param slices borrow `body`.
        if let Ok(map) = serde_json::from_slice::<indexmap::IndexMap<&str, &RawValue>>(body) {
            let replies = futures::future::join_all(map.into_iter().map(|(action, raw)| {
                let req = req.clone();
                async move {
                    let reply = self.dispatch_one_borrowed(req, action, raw.get()).await;
                    (action.to_owned(), reply)
                }
            }))
            .await;
            let mut out = Map::with_capacity(replies.len());
            for (action, reply) in replies {
                out.insert(action, reply.into_value());
            }
            return Value::Object(out);
        }
        // Array form: `[["action", params], …]` — ordered, duplicates allowed.
        if let Ok(items) = serde_json::from_slice::<Vec<(&str, &RawValue)>>(body) {
            let replies = futures::future::join_all(items.into_iter().map(|(action, raw)| {
                let req = req.clone();
                async move { self.dispatch_one_borrowed(req, action, raw.get()).await }
            }))
            .await;
            return Value::Array(replies.into_iter().map(Reply::into_value).collect());
        }
        Value::Object(Map::new())
    }

    /// Routes one borrowed call: a registered borrow action gets the raw param
    /// bytes (zero-copy); a plain owned action falls back through a single
    /// `Value` parse; otherwise `404`. Errors pass through `on_error`.
    async fn dispatch_one_borrowed(&self, req: R, action: &str, params: &str) -> Reply {
        let cx = Cx {
            app: Arc::clone(&self.app),
            req,
            client: ClientHandle::default(),
        };
        if let Some(handler) = self.borrow_actions.get(action) {
            return match handler.dispatch(&cx, params.as_bytes()).await {
                Ok(data) => Reply::ok(data),
                Err(error) => Reply::error((self.on_error)(error)),
            };
        }
        if let Some(handler) = self.actions.get(action) {
            let value = match serde_json::from_str::<Value>(params) {
                Ok(value) => value,
                Err(error) => return Reply::error((self.on_error)(Error::decode(&error))),
            };
            return match handler.dispatch(&cx, value).await {
                Ok(data) => Reply::ok(data),
                Err(error) => Reply::error((self.on_error)(error)),
            };
        }
        Reply::error((self.on_error)(Error::not_found(action)))
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
    /// Every error passes through the router's `on_error` hook first.
    async fn dispatch_one(&self, req: R, action: &str, params: Value) -> Reply {
        let Some(handler) = self.actions.get(action) else {
            return Reply::error((self.on_error)(Error::not_found(action)));
        };
        let cx = Cx {
            app: Arc::clone(&self.app),
            req,
            client: ClientHandle::default(),
        };
        match handler.dispatch(&cx, params).await {
            Ok(data) => Reply::ok(data),
            Err(error) => Reply::error((self.on_error)(error)),
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
        let on_error = Arc::clone(&self.on_error);
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
                    Arc::clone(&on_error),
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
        crate::ts::generate(
            &self.actions,
            &self.borrow_actions,
            &self.streams,
            &self.client_actions,
        )
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
    on_error: ErrorHook,
) -> impl Stream<Item = Frame>
where
    G: Send + Sync + 'static,
    R: Send + Sync + 'static,
{
    async_stream::stream! {
        let Some(handler) = handler else {
            yield Frame::error(index, action.clone(), (on_error)(Error::not_found(&action)));
            return;
        };
        let cx = Cx {
            app,
            req,
            client: ClientHandle::default(),
        };
        // `dispatch` runs the action's `before` guard + deserialize + validate
        // inside the stream; any of those failing surfaces as one `Err` item.
        let mut items = handler.dispatch(&cx, params);
        while let Some(item) = items.next().await {
            match item {
                Ok(value) => yield Frame::next(index, action.clone(), value),
                Err(error) => {
                    yield Frame::error(index, action, (on_error)(error));
                    return;
                }
            }
        }
        yield Frame::end(index, action);
    }
}

/// Parses an array payload `[["action", params], …]` into ordered entries.
///
/// Every input element maps to exactly one output entry, so the response array
/// stays index-aligned with the request. A malformed element (not a
/// `[string, value]` pair) becomes an empty action name, which dispatches to a
/// `404` error envelope in that slot rather than silently vanishing.
fn parse_array(items: Vec<Value>) -> Vec<(String, Value)> {
    items
        .into_iter()
        .map(|element| {
            let Value::Array(pair) = element else {
                return (String::new(), Value::Null);
            };
            let mut pair = pair.into_iter();
            let action = pair
                .next()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_default();
            let params = pair.next().unwrap_or(Value::Null);
            (action, params)
        })
        .collect()
}

/// Builder for [`Router`].
pub struct Builder<G, R> {
    app: Option<Arc<G>>,
    actions: HashMap<&'static str, Arc<dyn ErasedAction<G, R>>>,
    borrow_actions: HashMap<&'static str, Arc<dyn ErasedBorrowAction<G, R>>>,
    streams: HashMap<&'static str, Arc<dyn ErasedStreamAction<G, R>>>,
    client_actions: Vec<ClientMeta>,
    client_call_timeout: Duration,
    on_error: ErrorHook,
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

    /// Registers a **zero-copy** borrow action (keyed by its `name()`), dispatched
    /// via [`Router::on_request_borrowed`]. Its input borrows from the request
    /// buffer instead of owning copies. Additive — does not affect [`register`].
    ///
    /// [`register`]: Builder::register
    #[must_use]
    pub fn register_borrow<A: BorrowAction<G, R>>(mut self, action: A) -> Self {
        self.borrow_actions
            .insert(BorrowAction::name(&action), Arc::new(action));
        self
    }

    /// Registers a streaming action (keyed by its `name()`).
    #[must_use]
    pub fn register_stream<S: StreamAction<G, R>>(mut self, action: S) -> Self {
        self.streams
            .insert(StreamAction::name(&action), Arc::new(action));
        self
    }

    /// Sets how long a server→client `client_call` waits for the client's reply
    /// before failing with `504` (default [`DEFAULT_CLIENT_CALL_TIMEOUT`]). Bounds
    /// memory: a suspended action can't wait on a silent client forever.
    #[must_use]
    pub const fn client_call_timeout(mut self, timeout: Duration) -> Self {
        self.client_call_timeout = timeout;
        self
    }

    /// Installs an error hook: every outgoing error passes through `f` before
    /// reaching the client (`prev => new`; default is identity). Use it to redact
    /// sensitive detail in production while keeping rich errors in development —
    /// e.g. `.on_error(|e| if prod { Error::new(e.code, "error") } else { e })`.
    /// Runs at every error boundary: unary replies, stream error frames, and
    /// websocket `call` results.
    #[must_use]
    pub fn on_error<F>(mut self, f: F) -> Self
    where
        F: Fn(Error) -> Error + Send + Sync + 'static,
    {
        self.on_error = Arc::new(f);
        self
    }

    /// Declares a client action (server→client). Used by `cx.client_call::<C>()`
    /// and included in the generated TypeScript.
    #[must_use]
    pub fn client_action<C: ClientAction>(mut self) -> Self {
        let mut decls = std::collections::BTreeMap::new();
        <C::Params as TSType>::ts_declarations(&mut decls);
        <C::Return as TSType>::ts_declarations(&mut decls);
        self.client_actions.push(ClientMeta {
            name: C::NAME,
            params_ref: <C::Params as TSType>::ts_ref(),
            return_ref: <C::Return as TSType>::ts_ref(),
            decls,
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
            borrow_actions: self.borrow_actions,
            streams: self.streams,
            client_actions: self.client_actions,
            client_call_timeout: self.client_call_timeout,
            on_error: self.on_error,
        }
    }
}
