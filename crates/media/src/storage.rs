//! The storage seam: apps depend on this trait, tests mock it.

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;

use crate::MediaError;

/// A bucket of objects addressed by string keys.
#[async_trait]
pub trait ObjectStorage: Send + Sync {
    /// Store `bytes` under `key`, replacing anything there.
    async fn put(&self, key: &str, bytes: Bytes) -> Result<(), MediaError>;
    /// The object's bytes, or `None` when it does not exist.
    async fn get(&self, key: &str) -> Result<Option<Bytes>, MediaError>;
    /// Delete one object (deleting what is gone succeeds).
    async fn delete(&self, key: &str) -> Result<(), MediaError>;
    /// Every key starting with `prefix`.
    async fn list(&self, prefix: &str) -> Result<Vec<String>, MediaError>;
    /// The first-level "folders" (text before the first `/`).
    async fn folders(&self) -> Result<Vec<String>, MediaError>;
    /// A link anyone holding it can GET for `ttl`: one object, nothing else.
    /// `content_type` and `disposition` (e.g. `attachment; filename*=UTF-8''a.pdf`)
    /// override the response headers.
    fn presigned_get(
        &self,
        key: &str,
        ttl: Duration,
        content_type: Option<&str>,
        disposition: Option<&str>,
    ) -> String;
}
