//! The [`Client`] handle and its [`Builder`].

use std::sync::{Arc, RwLock};

use reqwest::Client as HttpClient;
use serde::Serialize;
use serde::de::DeserializeOwned;

use stakit_router::Endpoint;

use crate::options::CallOpts;
use crate::result::{ActionResult, Envelope};
use crate::{TransportError, pool};

/// A cheap, cloneable handle to a stakit server.
///
/// Holds a base url + base headers and shares a connection pool (one process-wide
/// pool by default). Cloning or building a handle is an `Arc` bump plus a couple
/// of `String`s — cheap enough to construct inside every action call. Use
/// per-call [`CallOpts`] to fan out to other servers without rebuilding.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

struct Inner {
    http: HttpClient,
    base_url: String,
    headers: RwLock<Vec<(String, String)>>,
    stream_url: Option<String>,
    ws_url: Option<String>,
    accept_invalid_certs: bool,
}

impl Client {
    /// Starts building a client with the given base url.
    pub fn builder(url: impl Into<String>) -> Builder {
        Builder {
            base_url: url.into(),
            headers: Vec::new(),
            http: None,
            stream_url: None,
            ws_url: None,
            accept_invalid_certs: false,
        }
    }

    /// Builds a client with default options (shared pool, no headers).
    pub fn new(url: impl Into<String>) -> Self {
        Self::builder(url).build()
    }

    /// Replaces the base headers wholesale (the object form of `setHeaders`).
    pub fn set_headers(&self, headers: Vec<(String, String)>) {
        *self.headers_lock_mut() = headers;
    }

    /// Updates the base headers in place (the functional form of `setHeaders`):
    /// you get the current headers and mutate them however you like.
    pub fn update_headers(&self, update: impl FnOnce(&mut Vec<(String, String)>)) {
        update(&mut self.headers_lock_mut());
    }

    /// Calls a unary action against the base url with default options.
    ///
    /// Returns `Err` only on a real transport failure; an application error from
    /// the action is `Ok(ActionResult::Error(..))`.
    ///
    /// # Errors
    /// See [`TransportError`].
    pub async fn fetch<E>(
        &self,
        endpoint: E,
        params: E::Params,
    ) -> Result<ActionResult<E::Output>, TransportError>
    where
        E: Endpoint,
        E::Params: Serialize,
        E::Output: DeserializeOwned,
    {
        self.fetch_with(endpoint, params, CallOpts::default()).await
    }

    /// Calls a unary action with per-call overrides (url, headers, files, method).
    ///
    /// # Errors
    /// See [`TransportError`].
    pub async fn fetch_with<E>(
        &self,
        _endpoint: E,
        params: E::Params,
        opts: CallOpts,
    ) -> Result<ActionResult<E::Output>, TransportError>
    where
        E: Endpoint,
        E::Params: Serialize,
        E::Output: DeserializeOwned,
    {
        let q = encode_query(E::ACTION, &params)?;
        let response = self.send_payload(q, opts).await?;
        let serde_json::Value::Object(mut map) = response else {
            return Err(TransportError::MissingAction(E::ACTION));
        };
        let entry = map
            .remove(E::ACTION)
            .ok_or(TransportError::MissingAction(E::ACTION))?;
        let envelope: Envelope<E::Output> =
            serde_json::from_value(entry).map_err(TransportError::Decode)?;
        Ok(envelope.into())
    }

    /// Sends a raw payload — an object `{action: params, …}` or an ordered array
    /// `[[action, params], …]` — and returns the response value verbatim (an
    /// object or array of envelopes). Lower-level than [`Client::batch`]; use it
    /// when you want full control over the payload shape.
    ///
    /// # Errors
    /// See [`TransportError`].
    pub async fn fetch_raw(
        &self,
        payload: serde_json::Value,
        opts: CallOpts,
    ) -> Result<serde_json::Value, TransportError> {
        let q = serde_json::to_string(&payload).map_err(TransportError::Encode)?;
        self.send_payload(q, opts).await
    }

    /// Starts a typed multi-action request: add several calls, then `send()` them
    /// in **one** round-trip. Results come back in order (the ordered-array
    /// payload, so the same action may be added more than once).
    ///
    /// ```ignore
    /// let results = client.batch()
    ///     .add(greet, Greet { name: "a".into() })
    ///     .add(version, ())
    ///     .send().await?;
    /// let g = results.get::<Greeting>(0)?;   // typed per index
    /// ```
    #[must_use]
    pub fn batch(&self) -> Batch<'_> {
        Batch {
            client: self,
            calls: Vec::new(),
            opts: CallOpts::default(),
            error: None,
        }
    }

    /// Issues the HTTP request for an encoded `q` payload and decodes the JSON
    /// response. Shared by [`Client::fetch_with`] and [`Client::fetch_raw`].
    async fn send_payload(
        &self,
        q: String,
        opts: CallOpts,
    ) -> Result<serde_json::Value, TransportError> {
        let has_files = !opts.files.is_empty();
        let method: reqwest::Method = opts.method.map_or_else(
            || {
                if has_files {
                    reqwest::Method::POST
                } else {
                    reqwest::Method::GET
                }
            },
            Into::into,
        );
        let url = opts.url.as_deref().unwrap_or(&self.inner.base_url);

        let mut request = self
            .inner
            .http
            .request(method, url)
            .query(&[("q", q.as_str())]);
        for (name, value) in self.merged_headers(&opts.headers) {
            request = request.header(name, value);
        }
        if has_files {
            let mut form = reqwest::multipart::Form::new();
            for bytes in opts.files {
                form = form.part(
                    "file",
                    reqwest::multipart::Part::bytes(bytes).file_name("file"),
                );
            }
            request = request.multipart(form);
        }

        let response = request.send().await?;
        response.json().await.map_err(Into::into)
    }

    /// The shared HTTP pool backing this handle.
    pub(crate) fn http(&self) -> &HttpClient {
        &self.inner.http
    }

    /// The base url.
    pub(crate) fn base_url(&self) -> &str {
        &self.inner.base_url
    }

    /// The configured stream url, if any.
    pub(crate) fn stream_url(&self) -> Option<&str> {
        self.inner.stream_url.as_deref()
    }

    /// The configured websocket url, if any.
    pub(crate) fn ws_url(&self) -> Option<&str> {
        self.inner.ws_url.as_deref()
    }

    /// Whether websocket TLS certificate verification is disabled.
    #[allow(
        clippy::missing_const_for_fn,
        reason = "deref through Arc is not const"
    )]
    pub(crate) fn accept_invalid_certs(&self) -> bool {
        self.inner.accept_invalid_certs
    }

    /// The base headers merged with per-call `extra` (extra wins on key clash).
    pub(crate) fn merged_headers(&self, extra: &[(String, String)]) -> Vec<(String, String)> {
        let mut headers = self
            .inner
            .headers
            .read()
            .expect("headers lock poisoned")
            .clone();
        for (name, value) in extra {
            if let Some(slot) = headers
                .iter_mut()
                .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
            {
                slot.1.clone_from(value);
            } else {
                headers.push((name.clone(), value.clone()));
            }
        }
        headers
    }

    fn headers_lock_mut(&self) -> std::sync::RwLockWriteGuard<'_, Vec<(String, String)>> {
        self.inner.headers.write().expect("headers lock poisoned")
    }
}

/// Encodes `{ action: params }` into the `q` query value (see `docs/transport.md`).
pub(crate) fn encode_query<P: Serialize>(
    action: &str,
    params: &P,
) -> Result<String, TransportError> {
    let mut map = serde_json::Map::new();
    map.insert(
        action.to_owned(),
        serde_json::to_value(params).map_err(TransportError::Encode)?,
    );
    serde_json::to_string(&serde_json::Value::Object(map)).map_err(TransportError::Encode)
}

/// A typed multi-action request builder (see [`Client::batch`]). Collects several
/// calls and sends them in one round-trip as an ordered-array payload.
pub struct Batch<'a> {
    client: &'a Client,
    calls: Vec<(String, serde_json::Value)>,
    opts: CallOpts,
    error: Option<TransportError>,
}

impl Batch<'_> {
    /// Adds a call. The same action may be added more than once (order is kept).
    #[must_use]
    pub fn add<E>(mut self, _endpoint: E, params: E::Params) -> Self
    where
        E: Endpoint,
        E::Params: Serialize,
    {
        if self.error.is_none() {
            match serde_json::to_value(&params) {
                Ok(value) => self.calls.push((E::ACTION.to_owned(), value)),
                Err(error) => self.error = Some(TransportError::Encode(error)),
            }
        }
        self
    }

    /// Applies per-call options (url / headers / files) to the whole batch.
    #[must_use]
    pub fn options(mut self, opts: CallOpts) -> Self {
        self.opts = opts;
        self
    }

    /// Sends every queued call in a single request; results come back in order.
    ///
    /// # Errors
    /// See [`TransportError`].
    pub async fn send(self) -> Result<BatchResults, TransportError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let payload = serde_json::Value::Array(
            self.calls
                .iter()
                .map(|(action, params)| {
                    serde_json::Value::Array(vec![
                        serde_json::Value::String(action.clone()),
                        params.clone(),
                    ])
                })
                .collect(),
        );
        let response = self.client.fetch_raw(payload, self.opts).await?;
        // We sent an ordered-array payload, so the server must answer with an
        // array of the same length; anything else is a protocol violation.
        let serde_json::Value::Array(envelopes) = response else {
            return Err(TransportError::UnexpectedResponse(
                "batch expected an array response",
            ));
        };
        let actions = self.calls.into_iter().map(|(action, _)| action).collect();
        Ok(BatchResults { actions, envelopes })
    }
}

/// The ordered results of a [`Batch::send`], decoded per index.
pub struct BatchResults {
    actions: Vec<String>,
    envelopes: Vec<serde_json::Value>,
}

impl BatchResults {
    /// Number of results.
    #[must_use]
    pub fn len(&self) -> usize {
        self.envelopes.len()
    }

    /// Whether the batch returned no results.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.envelopes.is_empty()
    }

    /// The action name at `index` (in the order added).
    #[must_use]
    pub fn action(&self, index: usize) -> Option<&str> {
        self.actions.get(index).map(String::as_str)
    }

    /// Decodes the result at `index` as a typed [`ActionResult`].
    ///
    /// # Errors
    /// `IndexOutOfRange` if `index` is past the end; `Decode` on a type mismatch.
    pub fn get<T: DeserializeOwned>(
        &self,
        index: usize,
    ) -> Result<ActionResult<T>, TransportError> {
        let value = self
            .envelopes
            .get(index)
            .ok_or(TransportError::IndexOutOfRange(index))?;
        let envelope: Envelope<T> =
            serde_json::from_value(value.clone()).map_err(TransportError::Decode)?;
        Ok(envelope.into())
    }
}

/// Builder for [`Client`].
pub struct Builder {
    base_url: String,
    headers: Vec<(String, String)>,
    http: Option<HttpClient>,
    stream_url: Option<String>,
    ws_url: Option<String>,
    accept_invalid_certs: bool,
}

impl Builder {
    /// Adds a base header sent on every call.
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Sets the url for the stream transport (defaults to the base url).
    #[must_use]
    pub fn stream_url(mut self, url: impl Into<String>) -> Self {
        self.stream_url = Some(url.into());
        self
    }

    /// Sets the url for the websocket transport (defaults to the base url).
    #[must_use]
    pub fn ws_url(mut self, url: impl Into<String>) -> Self {
        self.ws_url = Some(url.into());
        self
    }

    /// Uses a caller-provided connection pool instead of the shared one (custom
    /// TLS, proxies, isolated pools, …).
    #[must_use]
    pub fn pool(mut self, pool: HttpClient) -> Self {
        self.http = Some(pool);
        self
    }

    /// Disables websocket TLS certificate verification (`wss://`).
    ///
    /// **Dangerous** — only for trusted internal networks (self-signed / private
    /// CA, e.g. an internal microVM fleet). Never enable it against the public
    /// internet; it defeats MITM protection. Public `wss://` works without this.
    #[must_use]
    pub const fn danger_accept_invalid_certs(mut self, accept: bool) -> Self {
        self.accept_invalid_certs = accept;
        self
    }

    /// Finalizes the client.
    #[must_use]
    pub fn build(self) -> Client {
        Client {
            inner: Arc::new(Inner {
                http: self.http.unwrap_or_else(pool::shared),
                base_url: self.base_url,
                headers: RwLock::new(self.headers),
                stream_url: self.stream_url,
                ws_url: self.ws_url,
                accept_invalid_certs: self.accept_invalid_certs,
            }),
        }
    }
}
