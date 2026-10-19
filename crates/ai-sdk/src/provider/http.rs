//! HTTP plumbing shared by the built-in Claude and `OpenAI` providers: the
//! connection settings a client carries, request sending, and error mapping.

use serde_json::Value;

use crate::ProviderError;
use crate::provider::{ChatRequest, parse_retry_after};

/// Connection settings of a built-in provider client. The API key never
/// leaks through `{:?}`: clients format via [`HttpConfig::debug`].
#[derive(Clone)]
pub(crate) struct HttpConfig {
    pub(crate) api_key: String,
    pub(crate) base_url: String,
    pub(crate) endpoint: Option<String>,
    pub(crate) http: Option<reqwest::Client>,
}

impl HttpConfig {
    /// Settings for `api_key` against `base_url` with the shared default client.
    pub(crate) fn new(api_key: String, base_url: &str) -> Self {
        Self {
            api_key,
            base_url: base_url.to_owned(),
            endpoint: None,
            http: crate::provider::http_client(),
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

/// Passes a successful response through; turns a 4xx/5xx into
/// [`ProviderError::Api`] (used before decoding a stream).
pub(crate) async fn ensure_success(
    resp: reqwest::Response,
) -> Result<reqwest::Response, ProviderError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let retry_after = parse_retry_after(resp.headers());
    let text = resp.text().await.unwrap_or_default();
    Err(api_error(status.as_u16(), &text, retry_after))
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
