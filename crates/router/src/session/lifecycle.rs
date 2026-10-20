//! Per-session timer task: auth window, heartbeat pings, idle close.

use std::hash::{BuildHasher as _, RandomState};
use std::sync::Weak;
use std::time::Duration;

use bytes::Bytes;
use tokio::time::{Instant, MissedTickBehavior};

use super::{CloseReason, Shared};

const PING: Bytes = Bytes::from_static(br#"{"kind":"ping"}"#);

/// The only config the timer task keeps (not the whole `SessionConfig`).
pub(super) struct Timers {
    pub(super) heartbeat: Duration,
    pub(super) idle_timeout: Duration,
    /// `Some` when the session must authenticate within this window.
    pub(super) auth_window: Option<Duration>,
}

impl Timers {
    /// Runs until the session closes or is dropped (it holds only a `Weak`).
    pub(super) async fn run<G, R>(self, shared: Weak<Shared<G, R>>, seed: u64) {
        if let Some(window) = self.auth_window {
            tokio::time::sleep(window).await;
            let Some(session) = shared.upgrade() else {
                return;
            };
            if !session.is_authenticated() {
                session.close(CloseReason::AuthTimeout);
                return;
            }
        }
        // Up to 10% jitter so sessions opened together don't ping together.
        let tenth = u64::try_from(self.heartbeat.as_millis() / 10)
            .unwrap_or(0)
            .max(1);
        let jitter = Duration::from_millis(RandomState::new().hash_one(seed) % tenth);
        let first = Instant::now() + self.heartbeat + jitter;
        let mut ticker = tokio::time::interval_at(first, self.heartbeat);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let Some(session) = shared.upgrade() else {
                return;
            };
            if session.is_closed() {
                return;
            }
            if session.idle_for() >= self.idle_timeout {
                session.close(CloseReason::Idle);
                return;
            }
            session.send_now(PING);
        }
    }
}
