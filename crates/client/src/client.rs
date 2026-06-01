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
        let mut map: serde_json::Map<String, serde_json::Value> = response.json().await?;
        let entry = map
            .remove(E::ACTION)
            .ok_or(TransportError::MissingAction(E::ACTION))?;
        let envelope: Envelope<E::Output> =
            serde_json::from_value(entry).map_err(TransportError::Decode)?;
        Ok(envelope.into())
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

/// Builder for [`Client`].
pub struct Builder {
    base_url: String,
    headers: Vec<(String, String)>,
    http: Option<HttpClient>,
    stream_url: Option<String>,
    ws_url: Option<String>,
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
            }),
        }
    }
}
