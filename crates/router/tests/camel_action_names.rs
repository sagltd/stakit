//! Proves that under `--features camel` the **action name itself** (the
//! routing/dispatch key and the TypeScript interface keys) is converted to
//! `lowerCamelCase`, not only the struct field names.
//!
//! Run: `cargo test -p stakit-router --features camel`.
#![cfg(feature = "camel")]
#![allow(dead_code)]

use futures::executor::block_on;
use serde_json::{Value, json};

use stakit_router::{Endpoint, Error, Kind, Router, action, model};

// --- contexts ---
struct App;
#[derive(Clone)]
struct Req;

// --- models ---
#[model]
struct SomeUserThingParams {
    display_value: String,
}

#[model]
struct SomeUserThingResult {
    echo_value: String,
}

// A multi-word snake_case action name → must become `someUserThing`.
#[action]
async fn some_user_thing(params: SomeUserThingParams) -> Result<SomeUserThingResult, Error> {
    Ok(SomeUserThingResult {
        echo_value: params.display_value,
    })
}

// A two-word action → `acceptInvite`.
#[action]
async fn accept_invite(_params: SomeUserThingParams) -> Result<SomeUserThingResult, Error> {
    Ok(SomeUserThingResult {
        echo_value: String::new(),
    })
}

// A single-word action → stays `ping`.
#[action]
async fn ping() -> Result<SomeUserThingResult, Error> {
    Ok(SomeUserThingResult {
        echo_value: "pong".to_owned(),
    })
}

fn router() -> Router<App, Req> {
    Router::builder()
        .ctx(App)
        .register(some_user_thing)
        .register(accept_invite)
        .register(ping)
        .build()
}

/// Dispatches one action by key and returns its full envelope.
fn call(action: &str, params: Value) -> Value {
    let mut map = serde_json::Map::new();
    map.insert(action.to_owned(), params);
    let payload = Value::Object(map);
    let result = block_on(router().on_request(Req, payload));
    result[action].clone()
}

// ── Compile-time: `Endpoint::ACTION` constant is camelCase ──────────────────

/// The generated `const ACTION` is camelCase at compile time.
#[test]
fn endpoint_action_constant_is_camel_case() {
    assert_eq!(some_user_thing::ACTION, "someUserThing");
    assert_eq!(accept_invite::ACTION, "acceptInvite");
    assert_eq!(ping::ACTION, "ping");
}

/// `Kind` constant is still correct.
#[test]
fn endpoint_kind_constant_is_unary() {
    assert_eq!(some_user_thing::KIND, Kind::Unary);
}

// ── Runtime: dispatch under the camelCase key succeeds ──────────────────────

#[test]
fn dispatch_camel_key_some_user_thing_succeeds() {
    // Must dispatch under the camel key, not the snake key.
    let ok = call("someUserThing", json!({ "displayValue": "hello" }));
    assert_eq!(
        ok["status"], "ok",
        "dispatch under camelCase key failed: {ok}"
    );
    assert_eq!(ok["data"]["echoValue"], "hello");
}

#[test]
fn dispatch_snake_key_is_not_found() {
    // The snake-case key must no longer exist in the router.
    let err = call("some_user_thing", json!({ "displayValue": "hello" }));
    assert_eq!(
        err["error"]["code"], 404,
        "snake_case key must return 404 under camel feature: {err}"
    );
}

#[test]
fn dispatch_accept_invite_camel_succeeds() {
    let ok = call("acceptInvite", json!({ "displayValue": "x" }));
    assert_eq!(ok["status"], "ok", "acceptInvite dispatch failed: {ok}");
}

#[test]
fn dispatch_single_word_ping_succeeds() {
    let ok = call("ping", json!(null));
    assert_eq!(ok["status"], "ok", "ping dispatch failed: {ok}");
}

// ── Generated TypeScript: action keys are camelCase ─────────────────────────

#[test]
fn generated_ts_action_keys_are_camel_case() {
    let ts = router().generate_ts();

    // Action keys in the ActionParameters / ActionResults / ActionKinds maps.
    assert!(
        ts.contains("someUserThing:"),
        "expected `someUserThing:` in ActionParameters: {ts}"
    );
    assert!(
        ts.contains("acceptInvite:"),
        "expected `acceptInvite:` in TS: {ts}"
    );
    assert!(ts.contains("ping:"), "expected `ping:` in TS: {ts}");

    // Original snake_case action names must NOT appear as keys.
    assert!(
        !ts.contains("some_user_thing:"),
        "snake_case `some_user_thing:` leaked into TS: {ts}"
    );
    assert!(
        !ts.contains("accept_invite:"),
        "snake_case `accept_invite:` leaked into TS: {ts}"
    );
}

#[test]
fn generated_ts_struct_fields_are_also_camel_case() {
    let ts = router().generate_ts();
    // Field names on the models are still camelCase (existing behaviour).
    assert!(ts.contains("displayValue: string"), "{ts}");
    assert!(ts.contains("echoValue: string"), "{ts}");
    for snake in ["display_value", "echo_value"] {
        assert!(!ts.contains(snake), "leaked snake_case `{snake}`: {ts}");
    }
}
