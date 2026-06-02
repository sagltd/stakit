//! The [`Action`] trait and its type-erased form.

use std::future::Future;

use futures::Stream;
use futures::StreamExt as _;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use stakit_model::{Model, TSType, Validate};

use crate::{Cx, Error};

/// A typed action: validated input, typed output, run in a context.
///
/// Usually produced by `#[action]`; can be implemented by hand.
pub trait Action<G, R>: Send + Sync + 'static {
    /// Validated, deserializable input (`()` for param-less actions).
    type Input: Model + DeserializeOwned + Send;
    /// Serializable, TypeScript-exportable output.
    type Output: TSType + Serialize + Send;
    /// The action's own error type — anything convertible into [`Error`]
    /// (any `std::error::Error` works out of the box, defaulting to 500).
    type Error: Into<Error> + Send + 'static;

    /// The action's stable name (used for routing + TS).
    fn name(&self) -> &'static str;

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
    fn input_ts(&self) -> String;
    fn output_ts(&self) -> String;
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
    fn input_ts(&self) -> String {
        <A::Input as TSType>::to_ts()
    }

    fn output_ts(&self) -> String {
        <A::Output as TSType>::to_ts()
    }

    fn dispatch<'a>(
        &'a self,
        cx: &'a Cx<G, R>,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, Error>> {
        Box::pin(async move {
            let input: A::Input = serde_json::from_value(params).map_err(|e| Error::decode(&e))?;
            input.validate().map_err(Error::validation)?;
            let output = self.run(cx, input).await.map_err(Into::into)?;
            serde_json::to_value(output).map_err(|e| Error::encode(&e))
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
    /// Per-item error type — anything convertible into [`Error`].
    type Error: Into<Error> + Send + 'static;

    /// The action's stable name.
    fn name(&self) -> &'static str;

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
    fn input_ts(&self) -> String;
    fn item_ts(&self) -> String;
    fn dispatch<'a>(
        &'a self,
        cx: &'a Cx<G, R>,
        params: Value,
    ) -> Result<BoxStream<'a, Result<Value, Error>>, Error>;
}

impl<G, R, A> ErasedStreamAction<G, R> for A
where
    A: StreamAction<G, R>,
    G: Send + Sync + 'static,
    R: Send + Sync + 'static,
{
    fn input_ts(&self) -> String {
        <A::Input as TSType>::to_ts()
    }

    fn item_ts(&self) -> String {
        <A::Item as TSType>::to_ts()
    }

    fn dispatch<'a>(
        &'a self,
        cx: &'a Cx<G, R>,
        params: Value,
    ) -> Result<BoxStream<'a, Result<Value, Error>>, Error> {
        let input: A::Input = serde_json::from_value(params).map_err(|e| Error::decode(&e))?;
        input.validate().map_err(Error::validation)?;
        let stream = self.run(cx, input).map(|item| {
            item.map_err(Into::into)
                .and_then(|value| serde_json::to_value(value).map_err(|e| Error::encode(&e)))
        });
        Ok(Box::pin(stream))
    }
}
