//! Session lifecycle knobs, identity and close reasons.

use std::time::Duration;

use bytes::Bytes;
use serde_json::Value;

/// Lifecycle limits for every duplex session of a router. Set with
/// [`Builder::session_config`](crate::Builder::session_config).
#[derive(Clone, Debug)]
pub struct SessionConfig {
    /// How often the server sends a `{ kind: "ping" }` frame (default 25s).
    /// Clients answer `{ kind: "pong" }`; any inbound frame counts as alive.
    pub heartbeat: Duration,
    /// Close a session with no inbound frame for this long (default 60s).
    /// Checked on heartbeat ticks, so it fires within one `heartbeat` after.
    pub idle_timeout: Duration,
    /// With an auth hook installed, a session must authenticate (an `auth`
    /// frame) within this window or it is closed (default 5s).
    pub auth_window: Duration,
    /// Max concurrent sessions per authenticated user id (default 16).
    pub max_connections_per_user: usize,
    /// Max inbound frame size accepted by [`Session::handle_bytes`]
    /// (default 64 KiB). Configure your socket's max message size to match.
    ///
    /// [`Session::handle_bytes`]: crate::Session::handle_bytes
    pub max_frame_bytes: usize,
    /// Max client-requested subscriptions per session (default 128; the auto
    /// `user:` / `device:` channels don't count).
    pub max_subscriptions: usize,
    /// Outgoing frame queue per session (default 1024). When a publish finds
    /// it full the session is closed as a slow consumer.
    pub outgoing_buffer: usize,
    /// Max inbound frames per second per session (default 100); more closes
    /// the session ([`CloseReason::RateLimited`]).
    pub max_frames_per_second: u32,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            heartbeat: Duration::from_secs(25),
            idle_timeout: Duration::from_secs(60),
            auth_window: Duration::from_secs(5),
            max_connections_per_user: 16,
            max_frame_bytes: 64 * 1024,
            max_subscriptions: 128,
            outgoing_buffer: 1024,
            max_frames_per_second: 100,
        }
    }
}

/// Who a session belongs to.
///
/// A `user` gets the auto `user:id=<id>` channel (and counts toward the
/// per-user connection cap). A `device` needs a user: it
/// gets `device:user=<user>:id=<id>`, held by one connection at a time.
///
/// The auth hook must *prove* the device belongs to the user (e.g. a signed
/// device token) — never take a client-supplied device id on trust.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Identity {
    /// The user id.
    pub user: Option<String>,
    /// The device id.
    pub device: Option<String>,
}

impl Identity {
    /// An identity for `user`.
    #[must_use]
    pub fn user(user: impl Into<String>) -> Self {
        Self {
            user: Some(user.into()),
            device: None,
        }
    }

    /// Adds a device id.
    #[must_use]
    pub fn with_device(mut self, device: impl Into<String>) -> Self {
        self.device = Some(device.into());
        self
    }
}

/// Why the server ended a session. Read it from
/// [`Session::closed`](crate::Session::closed) and close the socket with
/// [`CloseReason::code`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    /// No `auth` frame within the auth window.
    AuthTimeout,
    /// The auth hook rejected the credentials.
    Unauthorized,
    /// The user already has the maximum number of sessions.
    TooManyConnections,
    /// No inbound frame (not even a `pong`) within the idle timeout.
    Idle,
    /// The outgoing queue was full: the client can't keep up. It should
    /// reconnect and resync.
    SlowConsumer,
    /// An inbound frame exceeded the max frame size.
    FrameTooLarge,
    /// The same user's device connected again (here or on another node); the
    /// older session steps down. Don't reconnect automatically.
    Superseded,
    /// The server revoked the user or device ([`Hub::close_user`] /
    /// [`Hub::close_device`]). Don't reconnect automatically.
    ///
    /// [`Hub::close_user`]: crate::Hub::close_user
    /// [`Hub::close_device`]: crate::Hub::close_device
    Revoked,
    /// More inbound frames per second than allowed.
    RateLimited,
}

impl CloseReason {
    /// The websocket close code to send (`1009` for oversized frames, `4xxx`
    /// application codes otherwise).
    #[must_use]
    pub const fn code(self) -> u16 {
        match self {
            Self::AuthTimeout => 4001,
            Self::Unauthorized => 4003,
            Self::TooManyConnections => 4029,
            Self::Idle => 4008,
            Self::SlowConsumer => 4009,
            Self::FrameTooLarge => 1009,
            Self::Superseded => 4010,
            Self::Revoked => 4011,
            Self::RateLimited => 1008,
        }
    }

    /// A short, stable reason string for the close frame.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AuthTimeout => "auth_timeout",
            Self::Unauthorized => "unauthorized",
            Self::TooManyConnections => "too_many_connections",
            Self::Idle => "idle",
            Self::SlowConsumer => "slow_consumer",
            Self::FrameTooLarge => "frame_too_large",
            Self::Superseded => "superseded",
            Self::Revoked => "revoked",
            Self::RateLimited => "rate_limited",
        }
    }
}

/// Serializes an outgoing frame once, ready for the socket.
pub(crate) fn encode(frame: &Value) -> Bytes {
    Bytes::from(serde_json::to_vec(frame).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_codes_are_distinct() {
        let reasons = [
            CloseReason::AuthTimeout,
            CloseReason::Unauthorized,
            CloseReason::TooManyConnections,
            CloseReason::Idle,
            CloseReason::SlowConsumer,
            CloseReason::FrameTooLarge,
            CloseReason::Superseded,
            CloseReason::Revoked,
            CloseReason::RateLimited,
        ];
        let mut codes: Vec<u16> = reasons.iter().map(|reason| reason.code()).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), reasons.len());
    }

    #[test]
    fn defaults_match_the_documented_limits() {
        let config = SessionConfig::default();
        assert_eq!(config.heartbeat, Duration::from_secs(25));
        assert_eq!(config.auth_window, Duration::from_secs(5));
        assert_eq!(config.max_frame_bytes, 65_536);
    }
}
