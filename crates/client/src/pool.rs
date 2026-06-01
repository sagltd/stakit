//! The shared connection pool.
//!
//! One process-wide [`reqwest::Client`] backs every [`Client`](crate::Client)
//! handle by default. reqwest pools connections per host internally, so a single
//! pool serves any number of target servers. Building a handle only clones this
//! pool's `Arc` — cheap enough to do inside every action call.

use std::sync::OnceLock;
use std::time::Duration;

use reqwest::Client as HttpClient;

/// Default end-to-end request timeout — a hung server must not hang the caller
/// forever (critical when fanning out to many microVMs). Override by supplying
/// your own pool via [`Builder::pool`](crate::Builder::pool).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Default TCP connect timeout.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

static SHARED: OnceLock<HttpClient> = OnceLock::new();

/// Returns the process-wide shared HTTP pool, building it on first use.
pub(crate) fn shared() -> HttpClient {
    SHARED
        .get_or_init(|| {
            HttpClient::builder()
                .timeout(DEFAULT_TIMEOUT)
                .connect_timeout(DEFAULT_CONNECT_TIMEOUT)
                .build()
                .expect("failed to build the shared reqwest client")
        })
        .clone()
}
