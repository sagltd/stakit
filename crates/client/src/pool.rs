//! The shared connection pool.
//!
//! One process-wide [`reqwest::Client`] backs every [`Client`](crate::Client)
//! handle by default. reqwest pools connections per host internally, so a single
//! pool serves any number of target servers. Building a handle only clones this
//! pool's `Arc` — cheap enough to do inside every action call.

use std::sync::OnceLock;

use reqwest::Client as HttpClient;

static SHARED: OnceLock<HttpClient> = OnceLock::new();

/// Returns the process-wide shared HTTP pool, building it on first use.
pub(crate) fn shared() -> HttpClient {
    SHARED
        .get_or_init(|| {
            HttpClient::builder()
                .build()
                .expect("failed to build the shared reqwest client")
        })
        .clone()
}
