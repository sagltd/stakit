//! Zero-copy borrow-action dispatch through the router.
//!
//! Proves the headline path: raw JSON bytes → `on_request_borrowed` → a
//! [`BorrowAction`] whose `&'a str` input **points into the request buffer**
//! (no copy), alongside the unchanged owned-action path (fallback).

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::executor::block_on;
use serde::{Deserialize, Serialize};

use stakit_model::Model;
use stakit_router::{BorrowAction, Cx, Error, Router, action};

// Lifetime-parameterized model: `email` borrows from the request buffer.
#[derive(Model, Serialize, Deserialize)]
struct LoginParams<'a> {
    #[validate(min_len = 1, max_len = 320, email)]
    email: &'a str,
}

#[derive(Model, Serialize, Deserialize)]
struct LoginResult {
    ok: bool,
    email: String,
}

/// A hand-written borrow action that records the address of its borrowed input
/// so the test can prove it pointed into the original request buffer.
struct Login {
    seen_ptr: Arc<AtomicUsize>,
}

impl<G, R> BorrowAction<G, R> for Login
where
    G: Send + Sync + 'static,
    R: Send + Sync + 'static,
{
    type Input<'de>
        = LoginParams<'de>
    where
        G: 'de,
        R: 'de;
    type Output = LoginResult;
    type Error = Error;

    fn name(&self) -> &'static str {
        "login"
    }

    async fn run<'a>(
        &'a self,
        _cx: &'a Cx<G, R>,
        input: LoginParams<'a>,
    ) -> Result<LoginResult, Error> {
        self.seen_ptr
            .store(input.email.as_ptr() as usize, Ordering::SeqCst);
        Ok(LoginResult {
            ok: true,
            email: input.email.to_owned(),
        })
    }
}

/// A plain owned action, to prove `on_request_borrowed` is a superset of
/// `on_request` (owned actions still dispatch through it).
#[action]
async fn ping() -> Result<String, Error> {
    Ok("pong".to_owned())
}

fn router(seen: Arc<AtomicUsize>) -> Router<(), ()> {
    Router::<(), ()>::builder()
        .ctx(())
        .register_borrow(Login { seen_ptr: seen })
        .register(ping)
        .build()
}

#[test]
fn router_dispatches_borrowed_action_and_returns_data() {
    let seen = Arc::new(AtomicUsize::new(0));
    let router = router(Arc::clone(&seen));

    let body = br#"{"login":{"email":"a@b.com"}}"#.to_vec();
    let response = block_on(router.on_request_borrowed((), &body));

    let data = &response["login"]["data"];
    assert_eq!(data["ok"], true);
    assert_eq!(data["email"], "a@b.com");
}

#[test]
fn borrowed_input_points_into_the_request_buffer() {
    let seen = Arc::new(AtomicUsize::new(0));
    let router = router(Arc::clone(&seen));

    let body = br#"{"login":{"email":"a@b.com"}}"#.to_vec();
    let _ = block_on(router.on_request_borrowed((), &body));

    // Zero-copy proof: the action saw an `email` pointer inside `body` — if the
    // router had built an owned `Value`, the string would live elsewhere.
    let start = body.as_ptr() as usize;
    let end = start + body.len();
    let ptr = seen.load(Ordering::SeqCst);
    assert!(
        (start..end).contains(&ptr),
        "borrowed action input must point into the request buffer (zero-copy)"
    );
}

#[test]
fn borrowed_action_runs_derived_validation() {
    let seen = Arc::new(AtomicUsize::new(0));
    let router = router(Arc::clone(&seen));

    let body = br#"{"login":{"email":"not-an-email"}}"#.to_vec();
    let response = block_on(router.on_request_borrowed((), &body));

    assert_eq!(response["login"]["status"], "error");
    assert_eq!(response["login"]["error"]["type"], "validation");
}

#[test]
fn owned_action_falls_back_through_borrowed_entrypoint() {
    let seen = Arc::new(AtomicUsize::new(0));
    let router = router(Arc::clone(&seen));

    // One payload, two calls: a borrow action and an owned action.
    let body = br#"{"login":{"email":"a@b.com"},"ping":null}"#.to_vec();
    let response = block_on(router.on_request_borrowed((), &body));

    assert_eq!(response["login"]["data"]["ok"], true);
    assert_eq!(response["ping"]["data"], "pong");
}

#[test]
fn array_payload_dispatches_borrowed_calls_in_order() {
    let seen = Arc::new(AtomicUsize::new(0));
    let router = router(Arc::clone(&seen));

    let body = br#"[["login",{"email":"a@b.com"}],["login",{"email":"c@d.com"}]]"#.to_vec();
    let response = block_on(router.on_request_borrowed((), &body));

    let array = response.as_array().unwrap();
    assert_eq!(array.len(), 2);
    assert_eq!(array[0]["data"]["email"], "a@b.com");
    assert_eq!(array[1]["data"]["email"], "c@d.com");
}

#[test]
fn unknown_borrowed_action_is_not_found() {
    let seen = Arc::new(AtomicUsize::new(0));
    let router = router(Arc::clone(&seen));

    let body = br#"{"nope":{"x":1}}"#.to_vec();
    let response = block_on(router.on_request_borrowed((), &body));

    assert_eq!(response["nope"]["status"], "error");
    assert_eq!(response["nope"]["error"]["code"], 404);
}

#[test]
fn borrow_action_appears_in_generated_typescript() {
    let seen = Arc::new(AtomicUsize::new(0));
    let ts = router(seen).generate_ts();

    assert!(ts.contains("export interface LoginParams {"), "{ts}");
    assert!(ts.contains("login: LoginParams;"), "{ts}");
    assert!(ts.contains("login: LoginResult;"), "{ts}");
}
