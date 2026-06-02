//! The [`Action`] trait and its type-erased form.

use std::future::Future;
use std::panic::AssertUnwindSafe;

use futures::FutureExt as _;
use futures::Stream;
use futures::StreamExt as _;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use stakit_model::{Model, TSType, Validate};

use crate::error::panic_message;
use crate::{Cx, Error, ErrorCodes};

/// A typed action: validated input, typed output, run in a context.
///
/// Usually produced by `#[action]`; can be implemented by hand.
pub trait Action<G, R>: Send + Sync + 'static {
    /// Validated, deserializable input (`()` for param-less actions).
    type Input: Model + DeserializeOwned + Send;
    /// Serializable, TypeScript-exportable output.
    type Output: TSType + Serialize + Send;
    /// The action's own error type: convertible into [`Error`] and exposing its
    /// machine codes for TypeScript generation. Deriving
    /// [`ResponseError`](crate::ResponseError) (usually with `thiserror`)
    /// supplies both; the router's own [`Error`] works too.
    type Error: Into<Error> + ErrorCodes + Send + 'static;

    /// The action's stable name (used for routing + TS).
    fn name(&self) -> &'static str;

    /// Hook run **before** params are deserialized/validated. `Err`
    /// short-circuits (the action never runs). Default: pass. Middleware guards
    /// override this, so an unauthorized caller is rejected before any input
    /// parsing — no validation-error schema leak past the gate.
    fn before<'a>(
        &'a self,
        _cx: &'a Cx<G, R>,
    ) -> impl Future<Output = Result<(), Error>> + Send + 'a {
        async { Ok(()) }
    }

    /// Hook run **after** the action completes (skipped if `before` failed).
    /// Default: no-op.
    fn after<'a>(&'a self, _cx: &'a Cx<G, R>) -> impl Future<Output = ()> + Send + 'a {
        async {}
    }

    /// Runs the action. Input is already validated. Returns a native future
    /// (no boxing) — the router boxes once at its dynamic-dispatch boundary.
    fn run<'a>(
        &'a self,
        cx: &'a Cx<G, R>,
        input: Self::Input,
    ) -> impl Future<Output = Result<Self::Output, Self::Error>> + Send + 'a;
}

/// Object-safe erasure so the router can hold heterogeneous actions by name.
pub(crate) trait ErasedAction<G, R>: Send + Sync {
    fn input_ref(&self) -> String;
    fn output_ref(&self) -> String;
    fn collect_ts(&self, out: &mut std::collections::BTreeMap<String, String>);
    fn error_codes(&self) -> &'static [&'static str];
    fn dispatch<'a>(
        &'a self,
        cx: &'a Cx<G, R>,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, Error>>;
}

impl<G, R, A> ErasedAction<G, R> for A
where
    A: Action<G, R>,
    G: Send + Sync + 'static,
    R: Send + Sync + 'static,
{
    fn input_ref(&self) -> String {
        <A::Input as TSType>::ts_ref()
    }

    fn output_ref(&self) -> String {
        <A::Output as TSType>::ts_ref()
    }

    fn collect_ts(&self, out: &mut std::collections::BTreeMap<String, String>) {
        <A::Input as TSType>::ts_declarations(out);
        <A::Output as TSType>::ts_declarations(out);
    }

    fn error_codes(&self) -> &'static [&'static str] {
        <A::Error as ErrorCodes>::error_codes()
    }

    fn dispatch<'a>(
        &'a self,
        cx: &'a Cx<G, R>,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, Error>> {
        // The whole pipeline runs inside `catch_unwind`: a panicking action (or
        // guard) becomes a `500` error instead of crashing the app/connection.
        Box::pin(async move {
            let work = AssertUnwindSafe(async move {
                self.before(cx).await?; // guard before deserialize/validate
                let input: A::Input =
                    serde_json::from_value(params).map_err(|e| Error::decode(&e))?;
                input.validate().map_err(Error::validation)?;
                let result = self.run(cx, input).await.map_err(Into::into);
                self.after(cx).await;
                serde_json::to_value(result?).map_err(|e| Error::encode(&e))
            });
            match work.catch_unwind().await {
                Ok(result) => result,
                Err(panic) => Err(Error::internal(format!(
                    "action panicked: {}",
                    panic_message(&*panic)
                ))),
            }
        })
    }
}

/// A streaming action: validated input, a stream of typed items.
///
/// Produced by `#[action(stream)]`. `cx.call(...)` works inside the body just
/// like a unary action.
pub trait StreamAction<G, R>: Send + Sync + 'static {
    /// Validated, deserializable input (`()` for param-less actions).
    type Input: Model + DeserializeOwned + Send;
    /// Serializable, TypeScript-exportable item type.
    type Item: TSType + Serialize + Send;
    /// Per-item error type: convertible into [`Error`] and exposing its machine
    /// codes for TypeScript generation (see [`Action::Error`]).
    type Error: Into<Error> + ErrorCodes + Send + 'static;

    /// The action's stable name.
    fn name(&self) -> &'static str;

    /// Hook run **before** params are deserialized/validated and the stream
    /// starts. `Err` short-circuits. Default: pass. (See [`Action::before`].)
    fn before<'a>(
        &'a self,
        _cx: &'a Cx<G, R>,
    ) -> impl Future<Output = Result<(), Error>> + Send + 'a {
        async { Ok(()) }
    }

    /// Hook run **after** the stream finishes normally (skipped if `before`
    /// failed or the stream is dropped early). Default: no-op.
    fn after<'a>(&'a self, _cx: &'a Cx<G, R>) -> impl Future<Output = ()> + Send + 'a {
        async {}
    }

    /// Produces the item stream. Input is already validated. Returns a native
    /// stream (no boxing) — the router boxes once at its dispatch boundary.
    fn run<'a>(
        &'a self,
        cx: &'a Cx<G, R>,
        input: Self::Input,
    ) -> impl Stream<Item = Result<Self::Item, Self::Error>> + Send + 'a;
}

/// Object-safe erasure for streaming actions.
pub(crate) trait ErasedStreamAction<G, R>: Send + Sync {
    fn input_ref(&self) -> String;
    fn item_ref(&self) -> String;
    fn collect_ts(&self, out: &mut std::collections::BTreeMap<String, String>);
    fn error_codes(&self) -> &'static [&'static str];
    fn dispatch<'a>(
        &'a self,
        cx: &'a Cx<G, R>,
        params: Value,
    ) -> BoxStream<'a, Result<Value, Error>>;
}

impl<G, R, A> ErasedStreamAction<G, R> for A
where
    A: StreamAction<G, R>,
    G: Send + Sync + 'static,
    R: Send + Sync + 'static,
{
    fn input_ref(&self) -> String {
        <A::Input as TSType>::ts_ref()
    }

    fn item_ref(&self) -> String {
        <A::Item as TSType>::ts_ref()
    }

    fn collect_ts(&self, out: &mut std::collections::BTreeMap<String, String>) {
        <A::Input as TSType>::ts_declarations(out);
        <A::Item as TSType>::ts_declarations(out);
    }

    fn error_codes(&self) -> &'static [&'static str] {
        <A::Error as ErrorCodes>::error_codes()
    }

    fn dispatch<'a>(
        &'a self,
        cx: &'a Cx<G, R>,
        params: Value,
    ) -> BoxStream<'a, Result<Value, Error>> {
        Box::pin(async_stream::stream! {
            // Guard before any deserialize/validate (no schema leak past it),
            // caught so a panicking guard becomes an error frame, not a crash.
            match AssertUnwindSafe(self.before(cx)).catch_unwind().await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    yield Err(error);
                    return;
                }
                Err(panic) => {
                    yield Err(Error::internal(format!("action panicked: {}", panic_message(&*panic))));
                    return;
                }
            }
            let input: A::Input = match serde_json::from_value(params) {
                Ok(input) => input,
                Err(error) => {
                    yield Err(Error::decode(&error));
                    return;
                }
            };
            if let Err(error) = input.validate().map_err(Error::validation) {
                yield Err(error);
                return;
            }
            let mut items = ::std::pin::pin!(self.run(cx, input));
            // Each item poll is caught: a panic mid-stream ends it with an error
            // frame rather than crashing.
            loop {
                match AssertUnwindSafe(items.next()).catch_unwind().await {
                    Ok(Some(item)) => {
                        yield item.map_err(Into::into).and_then(|value| {
                            serde_json::to_value(value).map_err(|e| Error::encode(&e))
                        });
                    }
                    Ok(None) => break,
                    Err(panic) => {
                        yield Err(Error::internal(format!("action panicked: {}", panic_message(&*panic))));
                        return;
                    }
                }
            }
            // `after` is caught too: a panicking teardown hook ends the stream
            // with an error frame instead of escaping the spawned task.
            if let Err(panic) = AssertUnwindSafe(self.after(cx)).catch_unwind().await {
                yield Err(Error::internal(format!("action panicked: {}", panic_message(&*panic))));
            }
        })
    }
}
