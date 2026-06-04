//! Divan benchmarks for router dispatch (deserialize → validate → run → serialize).
#![allow(dead_code)]

use divan::{Bencher, black_box};
use futures::StreamExt as _;
use futures::executor::block_on;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use stakit_model::Model;
use stakit_router::{BorrowAction, Cx, Error, Frame, Router, action};

fn main() {
    divan::main();
}

struct App;
#[derive(Clone)]
struct Req;

#[derive(Model, Serialize, Deserialize)]
struct Greet {
    #[validate(min_len = 1, max_len = 20)]
    name: String,
}

#[derive(Model, Serialize)]
struct Greeting {
    message: String,
}

#[action]
async fn greet(_cx: &Cx<App, Req>, params: Greet) -> Result<Greeting, Error> {
    Ok(Greeting {
        message: params.name,
    })
}

#[derive(Model, Serialize, Deserialize)]
struct Count {
    n: u64,
}

#[action(stream)]
fn count(params: Count) -> impl futures::Stream<Item = Result<u64, Error>> {
    async_stream::stream! {
        for i in 0..params.n {
            yield Ok(i);
        }
    }
}

// Minimal action: `()` params + `()` output, so its benchmark isolates the
// router's own overhead (payload parse + O(1) name lookup + envelope assembly)
// from serde/validation cost.
#[action]
async fn noop() -> Result<(), Error> {
    Ok(())
}

// Zero-copy twin of `greet`: `name` borrows from the request buffer. Output is
// the name length (a `u64`) so the benchmark isolates the *parse* difference —
// owned `Value` + field clone vs borrowed `from_slice` — not output allocation.
#[derive(Model, Serialize, Deserialize)]
struct GreetBorrow<'a> {
    #[validate(min_len = 1, max_len = 8192)]
    name: &'a str,
}

struct GreetB;

impl<G, R> BorrowAction<G, R> for GreetB
where
    G: Send + Sync + 'static,
    R: Send + Sync + 'static,
{
    type Input<'de>
        = GreetBorrow<'de>
    where
        G: 'de,
        R: 'de;
    type Output = u64;
    type Error = Error;

    fn name(&self) -> &'static str {
        "greetb"
    }

    async fn run<'a>(&'a self, _cx: &'a Cx<G, R>, input: GreetBorrow<'a>) -> Result<u64, Error> {
        Ok(input.name.len() as u64)
    }
}

fn router() -> Router<App, Req> {
    Router::builder()
        .ctx(App)
        .register(greet)
        .register(noop)
        .register_borrow(GreetB)
        .register_stream(count)
        .build()
}

/// A string-heavy single-call payload (a long `name`) where copy cost dominates.
fn big_payload(action: &str) -> Vec<u8> {
    let name = "x".repeat(2048);
    format!(r#"{{"{action}":{{"name":"{name}"}}}}"#).into_bytes()
}

/// Owned path as a real axum server runs it: parse the body into a `Value`, then
/// dispatch (which clones each `String` field out of the `Value`).
#[divan::bench]
fn owned_from_bytes(bencher: Bencher<'_, '_>) {
    let router = router();
    bencher
        .with_inputs(|| big_payload("greet"))
        .bench_values(|body: Vec<u8>| {
            let value: Value = serde_json::from_slice(&body).unwrap();
            black_box(block_on(router.on_request(Req, value)))
        });
}

/// Borrow path: deserialize straight from the bytes, `name` borrowing the buffer
/// — no intermediate `Value`, no per-field `String` allocation.
#[divan::bench]
fn borrowed_from_bytes(bencher: Bencher<'_, '_>) {
    let router = router();
    bencher
        .with_inputs(|| big_payload("greetb"))
        .bench_values(|body: Vec<u8>| black_box(block_on(router.on_request_borrowed(Req, &body))));
}

/// Full unary dispatch on valid input (single-call payload — the hot path).
#[divan::bench]
fn dispatch_valid(bencher: Bencher<'_, '_>) {
    let router = router();
    bencher
        .with_inputs(|| json!({ "greet": { "name": "bob" } }))
        .bench_values(|payload: Value| black_box(block_on(router.on_request(Req, payload))));
}

/// Router-only overhead: parse payload + O(1) name lookup + assemble the
/// response envelope, with negligible serde/validation cost.
#[divan::bench]
fn route_only(bencher: Bencher<'_, '_>) {
    let router = router();
    bencher
        .with_inputs(|| json!({ "noop": null }))
        .bench_values(|payload: Value| black_box(block_on(router.on_request(Req, payload))));
}

/// Dispatch on invalid input (validation error path).
#[divan::bench]
fn dispatch_invalid(bencher: Bencher<'_, '_>) {
    let router = router();
    bencher
        .with_inputs(|| json!({ "greet": { "name": "" } }))
        .bench_values(|payload: Value| black_box(block_on(router.on_request(Req, payload))));
}

/// Full streaming dispatch: 10 items + End, collected.
#[divan::bench]
fn dispatch_stream(bencher: Bencher<'_, '_>) {
    let router = router();
    bencher
        .with_inputs(|| json!({ "count": { "n": 10 } }))
        .bench_values(|payload: Value| {
            let frames: Vec<Frame> = block_on(router.on_stream(Req, payload).collect());
            black_box(frames)
        });
}
