//! A lightweight, cloneable cancellation token.
//!
//! Used to abort an in-flight provider call (and the agent loop) mid-stream.
//! Cloning shares the same cancellation state; cancelling wakes all waiters.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

/// A cloneable handle that signals cancellation to the agent loop / provider.
#[derive(Clone, Default)]
pub struct CancelToken {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancelToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

struct Inner {
    cancelled: AtomicBool,
    notify: Notify,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }
}

impl CancelToken {
    /// A fresh, un-cancelled token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Signals cancellation and wakes anything awaiting [`CancelToken::cancelled`].
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
        self.inner.notify.notify_waiters();
    }

    /// Whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// Resolves once the token is cancelled (immediately if already cancelled).
    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            let waiter = self.inner.notify.notified();
            if self.is_cancelled() {
                return;
            }
            waiter.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancel_resolves_waiters() {
        let token = CancelToken::new();
        assert!(!token.is_cancelled());
        let t2 = token.clone();
        let handle = tokio::spawn(async move { t2.cancelled().await });
        token.cancel();
        handle.await.expect("join");
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn already_cancelled_resolves_immediately() {
        let token = CancelToken::new();
        token.cancel();
        token.cancelled().await; // returns immediately
    }
}
