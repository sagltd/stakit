//! Per-call request options.

/// HTTP method override for a call. Defaults to `GET` (no files) or `POST`
/// (with files) when left unset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Method {
    /// `GET` — params in the query string, no body.
    #[default]
    Get,
    /// `POST`.
    Post,
    /// `PUT`.
    Put,
    /// `PATCH`.
    Patch,
    /// `DELETE`.
    Delete,
}

impl From<Method> for reqwest::Method {
    fn from(method: Method) -> Self {
        match method {
            Method::Get => Self::GET,
            Method::Post => Self::POST,
            Method::Put => Self::PUT,
            Method::Patch => Self::PATCH,
            Method::Delete => Self::DELETE,
        }
    }
}

/// Options applied to a **single** call. Every field is optional; what you set
/// overrides the client's base for that call only — the base is never mutated.
///
/// ```ignore
/// client.fetch_with(greet, params, CallOpts {
///     url: Some("https://vm-42.internal".into()),   // fan out to another server
///     headers: vec![("authorization".into(), token)], // merged over base headers
///     ..Default::default()
/// }).await?;
/// ```
#[derive(Debug, Clone, Default)]
pub struct CallOpts {
    /// Override the base url for this call (e.g. fan out to another server).
    pub url: Option<String>,
    /// Headers merged over the base headers (per-call wins on key clash).
    pub headers: Vec<(String, String)>,
    /// Files to upload as multipart `file` parts (HTTP only).
    pub files: Vec<Vec<u8>>,
    /// Override the HTTP method.
    pub method: Option<Method>,
}

impl CallOpts {
    /// An empty option set (same as `CallOpts::default()`).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the per-call url.
    #[must_use]
    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    /// Adds a per-call header (merged over the base headers).
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Adds a file to upload as a multipart `file` part.
    #[must_use]
    pub fn file(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.files.push(bytes.into());
        self
    }

    /// Sets the HTTP method.
    #[must_use]
    pub const fn method(mut self, method: Method) -> Self {
        self.method = Some(method);
        self
    }
}
