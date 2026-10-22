//! Media storage for apps: [`ObjectStorage`] (an S3-compatible bucket: Fly Tigris,
//! Cloudflare `R2`, `MinIO`, AWS) with presigned GETs, [`sniff`] (magic-byte content
//! types) and [`images`] (server-side thumbnails and blurhash placeholders).
//!
//! Everything fallible returns [`MediaError`]; nothing panics. Config example for
//! Tigris (what `fly storage create` sets):
//!
//! ```text
//! AWS_ENDPOINT_URL_S3=https://fly.storage.tigris.dev
//! AWS_ACCESS_KEY_ID=...
//! AWS_SECRET_ACCESS_KEY=...
//! AWS_REGION=auto
//! BUCKET_NAME=my-app-media
//! ```
//! [`S3Config::from_lookup`] reads exactly these.

pub mod error;
pub mod images;
pub mod s3;
pub mod sniff;
pub mod storage;

pub use error::MediaError;
pub use s3::{S3Config, S3Store};
pub use storage::ObjectStorage;
