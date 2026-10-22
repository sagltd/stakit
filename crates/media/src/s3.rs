//! [`ObjectStorage`] over the S3 API with minimal `SigV4` signing (`rusty-s3`; no AWS SDK).

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use rusty_s3::actions::{ListObjectsV2, S3Action as _};
use rusty_s3::{Bucket, Credentials, UrlStyle};
use url::Url;

use crate::{MediaError, ObjectStorage};

/// Signature lifetime of the server's own calls.
const CALL_TTL: Duration = Duration::from_secs(60);
/// One call may take at most this long.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const PAGE: usize = 1000;

/// Connection settings.
#[derive(Debug, Clone)]
pub struct S3Config {
    /// Endpoint, e.g. `https://fly.storage.tigris.dev` or `https://<id>.r2.cloudflarestorage.com`.
    pub endpoint: String,
    /// Region (`auto` for Tigris and R2).
    pub region: String,
    /// Access key id.
    pub access_key_id: String,
    /// Secret access key.
    pub secret_access_key: String,
    /// Bucket name.
    pub bucket: String,
}

impl S3Config {
    /// Read the Tigris-style variables (`AWS_ENDPOINT_URL_S3`, `AWS_ACCESS_KEY_ID`,
    /// `AWS_SECRET_ACCESS_KEY`, `AWS_REGION` (default `auto`), `BUCKET_NAME`).
    /// `None` when none of the credentials is set.
    ///
    /// # Errors
    /// [`MediaError::Config`] when only some are set.
    pub fn from_lookup(
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, MediaError> {
        let get = |key: &str| lookup(key).filter(|value| !value.trim().is_empty());
        let required = [
            "AWS_ENDPOINT_URL_S3",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "BUCKET_NAME",
        ]
        .map(get);
        if required.iter().all(Option::is_none) {
            return Ok(None);
        }
        let [
            Some(endpoint),
            Some(access_key_id),
            Some(secret_access_key),
            Some(bucket),
        ] = required
        else {
            return Err(MediaError::Config(
                "set AWS_ENDPOINT_URL_S3, AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY and BUCKET_NAME together"
                    .to_owned(),
            ));
        };
        Ok(Some(Self {
            endpoint,
            region: get("AWS_REGION").unwrap_or_else(|| "auto".to_owned()),
            access_key_id,
            secret_access_key,
            bucket,
        }))
    }
}

/// A bucket reached over HTTPS.
pub struct S3Store {
    bucket: Bucket,
    credentials: Credentials,
    http: reqwest::Client,
}

impl S3Store {
    /// A store for `config`.
    ///
    /// # Errors
    /// [`MediaError::Config`] for a malformed endpoint or bucket.
    pub fn new(config: &S3Config) -> Result<Self, MediaError> {
        let endpoint = Url::parse(&config.endpoint)
            .map_err(|error| MediaError::Config(format!("endpoint: {error}")))?;
        let bucket = Bucket::new(
            endpoint,
            UrlStyle::Path,
            config.bucket.clone(),
            config.region.clone(),
        )
        .map_err(|error| MediaError::Config(format!("bucket: {error}")))?;
        let http = reqwest::Client::builder()
            .timeout(CALL_TIMEOUT)
            .build()
            .map_err(|error| MediaError::Config(format!("http client: {error}")))?;
        Ok(Self {
            bucket,
            credentials: Credentials::new(&config.access_key_id, &config.secret_access_key),
            http,
        })
    }

    async fn list_page(
        &self,
        prefix: &str,
        delimiter: bool,
    ) -> Result<(Vec<String>, Vec<String>), MediaError> {
        let (mut keys, mut folders) = (Vec::new(), Vec::new());
        let mut token: Option<String> = None;
        loop {
            let mut action = self.bucket.list_objects_v2(Some(&self.credentials));
            action.with_prefix(prefix);
            action.with_max_keys(PAGE);
            if delimiter {
                action.with_delimiter("/");
            }
            if let Some(token) = token.as_deref() {
                action.with_continuation_token(token);
            }
            let body = self
                .http
                .get(action.sign(CALL_TTL))
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
                .map_err(|error| MediaError::store("list", error))?
                .text()
                .await
                .map_err(|error| MediaError::store("list", error))?;
            let page = ListObjectsV2::parse_response(&body)
                .map_err(|error| MediaError::store("list", error))?;
            keys.extend(page.contents.into_iter().map(|object| object.key));
            folders.extend(
                page.common_prefixes
                    .into_iter()
                    .map(|folder| folder.prefix.trim_end_matches('/').to_owned()),
            );
            token = page.next_continuation_token;
            if token.is_none() {
                return Ok((keys, folders));
            }
        }
    }
}

#[async_trait]
impl ObjectStorage for S3Store {
    async fn put(&self, key: &str, bytes: Bytes) -> Result<(), MediaError> {
        let url = self
            .bucket
            .put_object(Some(&self.credentials), key)
            .sign(CALL_TTL);
        self.http
            .put(url)
            .body(bytes)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map(|_| ())
            .map_err(|error| MediaError::store("put", error))
    }

    async fn get(&self, key: &str) -> Result<Option<Bytes>, MediaError> {
        let url = self
            .bucket
            .get_object(Some(&self.credentials), key)
            .sign(CALL_TTL);
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|error| MediaError::store("get", error))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        response
            .error_for_status()
            .map_err(|error| MediaError::store("get", error))?
            .bytes()
            .await
            .map(Some)
            .map_err(|error| MediaError::store("get", error))
    }

    async fn delete(&self, key: &str) -> Result<(), MediaError> {
        let url = self
            .bucket
            .delete_object(Some(&self.credentials), key)
            .sign(CALL_TTL);
        self.http
            .delete(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map(|_| ())
            .map_err(|error| MediaError::store("delete", error))
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>, MediaError> {
        Ok(self.list_page(prefix, false).await?.0)
    }

    async fn folders(&self) -> Result<Vec<String>, MediaError> {
        Ok(self.list_page("", true).await?.1)
    }

    fn presigned_get(
        &self,
        key: &str,
        ttl: Duration,
        content_type: Option<&str>,
        disposition: Option<&str>,
    ) -> String {
        let mut action = self.bucket.get_object(Some(&self.credentials), key);
        if let Some(content_type) = content_type {
            action
                .query_mut()
                .insert("response-content-type", content_type.to_owned());
        }
        if let Some(disposition) = disposition {
            action
                .query_mut()
                .insert("response-content-disposition", disposition.to_owned());
        }
        action.sign(ttl).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> S3Store {
        S3Store::new(&S3Config {
            endpoint: "https://fly.storage.tigris.dev".to_owned(),
            region: "auto".to_owned(),
            access_key_id: "AKID".to_owned(),
            secret_access_key: "secret".to_owned(),
            bucket: "media".to_owned(),
        })
        .expect("store")
    }

    #[test]
    fn a_presigned_link_names_one_object_and_expires() {
        let link = store().presigned_get(
            "c1/a1",
            Duration::from_secs(600),
            Some("application/pdf"),
            Some("attachment; filename*=UTF-8''a.pdf"),
        );
        let url = Url::parse(&link).expect("url");
        assert_eq!(url.host_str(), Some("fly.storage.tigris.dev"));
        assert_eq!(url.path(), "/media/c1/a1");
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query["X-Amz-Expires"], "600");
        assert!(query.contains_key("X-Amz-Signature"));
        assert_eq!(query["response-content-type"], "application/pdf");
    }

    #[test]
    fn different_objects_get_different_signatures() {
        let one = store().presigned_get("c1/a", Duration::from_secs(60), None, None);
        let two = store().presigned_get("c1/b", Duration::from_secs(60), None, None);
        assert_ne!(one, two);
    }

    fn lookup(skip: Option<&'static str>) -> impl Fn(&str) -> Option<String> {
        move |key| {
            let known = [
                "AWS_ENDPOINT_URL_S3",
                "AWS_ACCESS_KEY_ID",
                "AWS_SECRET_ACCESS_KEY",
                "BUCKET_NAME",
            ];
            (known.contains(&key) && Some(key) != skip).then(|| "https://x.example".to_owned())
        }
    }

    #[test]
    fn config_reads_the_tigris_variables() {
        let config = S3Config::from_lookup(lookup(None))
            .expect("ok")
            .expect("some");
        assert_eq!(config.region, "auto");
        assert!(S3Config::from_lookup(|_| None).expect("ok").is_none());
        assert!(S3Config::from_lookup(lookup(Some("BUCKET_NAME"))).is_err());
    }
}
