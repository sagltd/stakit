//! Divan benchmarks for router dispatch (deserialize → validate → run → serialize).
#![allow(dead_code)]

use divan::{Bencher, black_box};
use futures::StreamExt as _;
use futures::executor::block_on;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use stakit_model::Model;
use stakit_router::{Cx, Error, Frame, Router, action};

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

fn router() -> Router<App, Req> {
    Router::builder()
        .ctx(App)
        .register(greet)
        .register(noop)
        .register_stream(count)
        .build()
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
