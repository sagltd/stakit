//! Per-action middleware (guards): run a check before an action — and optionally
//! a hook after — without reaching the action body if the guard fails.
//!
//! Trait-based, plain `async fn`, no boxing:
//!
//! ```ignore
//! struct RequireAdmin;
//! impl Middleware<App, Req> for RequireAdmin {
//!     async fn before(&self, cx: &Cx<App, Req>) -> Result<(), Error> {
//!         if cx.req.admin { Ok(()) } else { Err(err!(403, "admin only")) }
//!     }
//! }
//!
//! router.register(greet.middleware(RequireAdmin));
//! ```
//!
//! The guard runs inside the action's `run`, i.e. *after* input deserialize +
//! validation — it short-circuits the action body, not the whole pipeline.

use std::future::Future;

use futures::StreamExt as _;
use futures::future::BoxFuture;
use futures::stream::BoxStream;

use crate::{Action, Cx, Error, StreamAction};

/// A pre/post-action guard. Implement `before` (default: pass) to gate the
/// action; `after` (default: no-op) runs once the action completes.
///
/// Both are plain `async fn`. Returning `Err` from `before` short-circuits — the
/// action body never runs and the error becomes the reply.
pub trait Middleware<G, R>: Send + Sync + 'static {
    /// Runs before the action. `Err` skips the action and is returned instead.
    fn before(&self, _cx: &Cx<G, R>) -> impl Future<Output = Result<(), Error>> + Send {
        async { Ok(()) }
    }

    /// Runs after the action completes (skipped if `before` short-circuited).
    fn after(&self, _cx: &Cx<G, R>) -> impl Future<Output = ()> + Send {
        async {}
    }
}

/// Attaches a [`Middleware`] to a unary action. Import it for `action.middleware(..)`.
pub trait ActionExt<G, R>: Action<G, R> + Sized {
    /// Wraps this action so `mw` guards it.
    fn middleware<M: Middleware<G, R>>(self, mw: M) -> Guarded<Self, M> {
        Guarded { action: self, mw }
    }
}

impl<G, R, A: Action<G, R>> ActionExt<G, R> for A {}

/// Attaches a [`Middleware`] to a streaming action.
pub trait StreamActionExt<G, R>: StreamAction<G, R> + Sized {
    /// Wraps this streaming action so `mw` guards it.
    fn middleware<M: Middleware<G, R>>(self, mw: M) -> StreamGuarded<Self, M> {
        StreamGuarded { action: self, mw }
    }
}

impl<G, R, A: StreamAction<G, R>> StreamActionExt<G, R> for A {}

/// A unary action wrapped with a [`Middleware`] guard (see [`ActionExt::middleware`]).
pub struct Guarded<A, M> {
    action: A,
    mw: M,
}

impl<G, R, A, M> Action<G, R> for Guarded<A, M>
where
    G: Send + Sync + 'static,
    R: Send + Sync + 'static,
    A: Action<G, R>,
    M: Middleware<G, R>,
{
    type Input = A::Input;
    type Output = A::Output;
    type Error = Error;

    fn name(&self) -> &'static str {
        self.action.name()
    }

    fn run<'a>(
        &'a self,
        cx: &'a Cx<G, R>,
        input: Self::Input,
    ) -> BoxFuture<'a, Result<Self::Output, Self::Error>> {
        Box::pin(async move {
            self.mw.before(cx).await?;
            let result = self.action.run(cx, input).await.map_err(Into::into);
            self.mw.after(cx).await;
            result
        })
    }
}

/// A streaming action wrapped with a [`Middleware`] guard (see
/// [`StreamActionExt::middleware`]).
pub struct StreamGuarded<A, M> {
    action: A,
    mw: M,
}

impl<G, R, A, M> StreamAction<G, R> for StreamGuarded<A, M>
where
    G: Send + Sync + 'static,
    R: Send + Sync + 'static,
    A: StreamAction<G, R>,
    M: Middleware<G, R>,
{
    type Input = A::Input;
    type Item = A::Item;
    type Error = Error;

    fn name(&self) -> &'static str {
        self.action.name()
    }

    fn run<'a>(
        &'a self,
        cx: &'a Cx<G, R>,
        input: Self::Input,
    ) -> BoxStream<'a, Result<Self::Item, Self::Error>> {
        Box::pin(async_stream::stream! {
            if let Err(error) = self.mw.before(cx).await {
                yield Err(error);
                return;
            }
            let mut items = self.action.run(cx, input);
            while let Some(item) = items.next().await {
                yield item.map_err(Into::into);
            }
            self.mw.after(cx).await;
        })
    }
}
