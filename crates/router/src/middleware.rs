//! Per-action middleware (guards): run a check before an action — and optionally
//! a hook after — without reaching the action body if the guard fails.
//!
//! Trait-based, plain `async fn`, no boxing in your code (the trait methods are
//! return-position `impl Future + Send`, which you satisfy with `async fn`):
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

use futures::Stream;

use crate::{Action, Cx, Error, StreamAction};

/// A pre/post-action guard. Implement `before` (default: pass) to gate the
/// action; `after` (default: no-op) runs once the action completes.
///
/// Both are plain `async fn` (no boxing). Returning `Err` from `before`
/// short-circuits — the action body never runs and the error becomes the reply;
/// `after` is then skipped too.
pub trait Middleware<G, R>: Send + Sync + 'static {
    /// Runs before the action. `Err` skips the action and is returned instead.
    fn before(&self, _cx: &Cx<G, R>) -> impl Future<Output = Result<(), Error>> + Send {
        async { Ok(()) }
    }

    /// Runs after the action completes. Skipped if `before` short-circuited.
    ///
    /// Best-effort, not a cleanup guarantee: for a unary action it runs whether
    /// the action returned `Ok` or `Err`; for a **stream** it runs only after the
    /// stream finishes *normally* — if the consumer drops the stream early (e.g.
    /// the client disconnects mid-stream) `after` does not run, because Rust has
    /// no async drop. Use it for logging/metrics, not for must-run teardown.
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
    type Error = A::Error;

    fn name(&self) -> &'static str {
        self.action.name()
    }

    // The guard rides the `before`/`after` hooks (which the router runs *before*
    // deserialize/validate), so a rejected caller never reaches parsing/the body.
    async fn before<'a>(&'a self, cx: &'a Cx<G, R>) -> Result<(), Error> {
        self.action.before(cx).await?;
        self.mw.before(cx).await
    }

    async fn after<'a>(&'a self, cx: &'a Cx<G, R>) {
        self.mw.after(cx).await;
        self.action.after(cx).await;
    }

    fn run<'a>(
        &'a self,
        cx: &'a Cx<G, R>,
        input: Self::Input,
    ) -> impl Future<Output = Result<Self::Output, Self::Error>> + Send + 'a {
        self.action.run(cx, input)
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
    type Error = A::Error;

    fn name(&self) -> &'static str {
        self.action.name()
    }

    async fn before<'a>(&'a self, cx: &'a Cx<G, R>) -> Result<(), Error> {
        self.action.before(cx).await?;
        self.mw.before(cx).await
    }

    async fn after<'a>(&'a self, cx: &'a Cx<G, R>) {
        self.mw.after(cx).await;
        self.action.after(cx).await;
    }

    fn run<'a>(
        &'a self,
        cx: &'a Cx<G, R>,
        input: Self::Input,
    ) -> impl Stream<Item = Result<Self::Item, Self::Error>> + Send + 'a {
        self.action.run(cx, input)
    }
}
