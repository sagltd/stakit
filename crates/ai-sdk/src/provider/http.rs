//! HTTP plumbing shared by the built-in Claude and `OpenAI` providers: the
//! connection settings a client carries, request sending, and error mapping.

use futures::StreamExt as _;
use serde_json::Value;

use crate::ProviderError;
use crate::provider::{ChatRequest, DEFAULT_RESPONSE_BYTE_LIMIT, parse_retry_after};

const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// Connection settings of a built-in provider client. The API key never
/// leaks through `{:?}`: clients format via [`HttpConfig::debug`].
#[derive(Clone)]
pub(crate) struct HttpConfig {
    pub(crate) api_key: String,
    pub(crate) base_url: String,
    pub(crate) endpoint: Option<String>,
    pub(crate) http: Option<reqwest::Client>,
    pub(crate) response_byte_limit: usize,
}

impl HttpConfig {
    /// Settings for `api_key` against `base_url` with the shared default client.
    pub(crate) fn new(api_key: String, base_url: &str) -> Self {
        Self {
            api_key,
            base_url: base_url.to_owned(),
            endpoint: None,
            http: crate::provider::http_client(),
            response_byte_limit: DEFAULT_RESPONSE_BYTE_LIMIT,
        }
    }

    /// Reads the API key from the environment variable `var`.
    pub(crate) fn key_from_env(var: &str) -> Result<String, ProviderError> {
        std::env::var(var).map_err(|_| ProviderError::InvalidArgument(format!("{var} is not set")))
    }

    /// Formats the owning client `name` with the API key redacted.
    pub(crate) fn debug(&self, name: &str, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(name)
            .field("api_key", &"[redacted]")
            .field("base_url", &self.base_url)
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }

    /// Starts a JSON POST to the endpoint override, else `{base_url}{path}`.
    pub(crate) fn post(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<reqwest::RequestBuilder, ProviderError> {
        let http = self.http.as_ref().ok_or_else(|| {
            ProviderError::Transport("provider HTTP client initialization failed".into())
        })?;
        let url = self
            .endpoint
            .clone()
            .unwrap_or_else(|| format!("{}{path}", self.base_url));
        Ok(http.post(url).json(body))
    }
}

/// Generates the builder methods every built-in client shares; the client
/// must hold its [`HttpConfig`] in a field named `config`.
macro_rules! http_client_builders {
    (base_url_doc: $base_url_doc:literal, path: $path:literal) => {
        #[doc = $base_url_doc]
        #[must_use]
        pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
            self.config.base_url = base_url.into();
            self
        }

        #[doc = concat!("Overrides the full request URL (instead of `{base_url}", $path, "`), for")]
        /// compatible APIs mounted under a different path, e.g. Gemini's
        /// `https://generativelanguage.googleapis.com/v1beta/openai/chat/completions`.
        #[must_use]
        pub fn with_endpoint(mut self, url: impl Into<String>) -> Self {
            self.config.endpoint = Some(url.into());
            self
        }

        /// Caps a successful non-streamed HTTP response body as bytes arrive.
        /// Defaults to [`crate::DEFAULT_RESPONSE_BYTE_LIMIT`] (8 MiB). Error
        /// bodies use the smaller of this limit and 64 KiB; oversized errors
        /// preserve status and retry hints but omit the provider message.
        /// A zero limit rejects any nonempty body. Successful SSE streams use
        /// the separate `MAX_STREAM_*` payload limits, not this total-byte cap.
        /// These limits exclude transport, allocator and parsed JSON overhead.
        #[must_use]
        pub const fn with_response_byte_limit(mut self, limit: usize) -> Self {
            self.config.response_byte_limit = limit;
            self
        }

        /// Uses `http` for every request instead of the shared default client, e.g.
        /// one with a custom DNS resolver or proxy. The caller owns its retry and
        /// redirect policy.
        #[must_use]
        pub fn with_http_client(mut self, http: reqwest::Client) -> Self {
            self.config.http = Some(http);
            self
        }
    };
}
pub(crate) use http_client_builders;

/// Sends a prepared request, mapping transport failures.
pub(crate) async fn send(
    request: Result<reqwest::RequestBuilder, ProviderError>,
) -> Result<reqwest::Response, ProviderError> {
    request?
        .send()
        .await
        .map_err(|e| ProviderError::Transport(e.to_string()))
}

/// Builds an [`ProviderError::Api`] from a 4xx/5xx response.
///
/// Extracts only the provider's structured `error.type` / `error.message`; the
/// raw `body` is never stored, since it can echo back request material
/// (including the API key) and would surface via `Display` into the run
/// outcome. When the body lacks the expected shape, falls back to a concise,
/// safe message.
pub(crate) fn api_error(
    status: u16,
    body: &str,
    retry_after: Option<std::time::Duration>,
) -> ProviderError {
    let parsed = serde_json::from_str::<Value>(body).ok();
    let kind = parsed
        .as_ref()
        .and_then(|v| v["error"]["type"].as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "api_error".to_owned());
    let message = parsed
        .as_ref()
        .and_then(|v| v["error"]["message"].as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| format!("provider error: {status} {kind}"));
    ProviderError::Api {
        status,
        kind,
        message,
        retry_after,
    }
}

/// Reads at most `limit` bytes, rejecting excess before appending the chunk.
/// The response is dropped immediately on failure, even without EOF or length.
pub(crate) async fn read_response_bytes(
    resp: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, ProviderError> {
    let mut body = Vec::new();
    let mut chunks = resp.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|error| ProviderError::Transport(error.to_string()))?;
        if chunk.len() > limit.saturating_sub(body.len()) {
            return Err(ProviderError::Decode {
                err: format!("provider response body exceeds {limit} bytes"),
                body: String::new(),
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Passes a successful response through; turns a 4xx/5xx into
/// [`ProviderError::Api`] (used before decoding a stream).
pub(crate) async fn ensure_success(
    resp: reqwest::Response,
    limit: usize,
) -> Result<reqwest::Response, ProviderError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let retry_after = parse_retry_after(resp.headers());
    let body = read_response_bytes(resp, limit.min(MAX_ERROR_BODY_BYTES))
        .await
        .unwrap_or_default();
    Err(api_error(
        status.as_u16(),
        &String::from_utf8_lossy(&body),
        retry_after,
    ))
}

/// Validates status and decodes one bounded JSON response without retaining
/// provider content in decoding errors.
pub(crate) async fn read_response_json(
    response: reqwest::Response,
    limit: usize,
) -> Result<Value, ProviderError> {
    let response = ensure_success(response, limit).await?;
    let bytes = read_response_bytes(response, limit).await?;
    serde_json::from_slice(&bytes).map_err(|error| ProviderError::Decode {
        err: format!(
            "invalid provider response JSON at line {} column {}",
            error.line(),
            error.column()
        ),
        body: String::new(),
    })
}

/// The request's model id when set, else the handle's bound `model`.
pub(crate) fn pick_model(model: &str, request: &ChatRequest) -> String {
    if request.model.is_empty() {
        model.to_owned()
    } else {
        request.model.clone()
    }
}

/// The unsigned counter `key` of a wire usage object, `0` when absent.
pub(crate) fn wire_count(usage: &Value, key: &str) -> u64 {
    usage.get(key).and_then(Value::as_u64).unwrap_or(0)
}

#[cfg(test)]
mod http_test;
