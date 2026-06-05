//! End-to-end proof of the `camel` feature through the **router**: declared with
//! the `#[model]` attribute, `snake_case` Rust fields surface as `camelCase` in
//! every place a client sees them — the wire JSON the router decodes, the
//! validation-error paths it reports, and the generated TypeScript — including
//! through generic, monomorphized types.
//!
//! Run: `cargo test -p stakit-router --features camel`.
#![cfg(feature = "camel")]
#![allow(dead_code)]

use futures::executor::block_on;
use serde_json::{Value, json};

use stakit_router::{Cx, Error, Router, action, model};

// --- contexts ---
struct App;
#[derive(Clone)]
struct Req;

// --- models declared with `#[model]` (injects `rename_all = "camelCase"` under
// the `camel` feature, so the wire format matches the camelCase TypeScript) ---

#[model]
struct CreateUser {
    #[validate(min_len = 3)]
    user_name: String,
    last_login_at: u64,
}

#[model]
struct User {
    id: u64,
    display_name: String,
}

// A generic envelope: proves the rename propagates through monomorphization.
#[model]
struct Message<T> {
    is_success: bool,
    data: T,
}

#[action]
async fn create_user(params: CreateUser) -> Result<Message<User>, Error> {
    Ok(Message {
        is_success: true,
        data: User {
            id: 1,
            display_name: params.user_name,
        },
    })
}

// Action-to-action: re-validates the (already typed) params under camel.
#[action]
async fn register_user(cx: &Cx<App, Req>, params: CreateUser) -> Result<Message<User>, Error> {
    cx.call(create_user, params).await
}

// An enum return: proves externally-tagged variant payload fields are camelCase
// on the wire through the router (tag stays verbatim).
#[model]
enum Notice {
    Welcome { user_name: String },
    Bye,
}

#[action]
async fn announce(params: CreateUser) -> Result<Notice, Error> {
    Ok(Notice::Welcome {
        user_name: params.user_name,
    })
}

// Nested `dive`: the validation path must camelCase every segment.
#[model]
struct LineItem {
    #[validate(min_len = 3)]
    item_name: String,
}

#[model]
struct Basket {
    #[validate(dive)]
    line_items: Vec<LineItem>,
}

#[action]
async fn checkout(_params: Basket) -> Result<User, Error> {
    Ok(User {
        id: 1,
        display_name: "ok".to_owned(),
    })
}

fn router() -> Router<App, Req> {
    Router::builder()
        .ctx(App)
        .register(create_user)
        .register(register_user)
        .register(announce)
        .register(checkout)
        .build()
}

/// One-call object payload `{ action: params }`.
fn payload(action: &str, params: Value) -> Value {
    let mut map = serde_json::Map::new();
    map.insert(action.to_owned(), params);
    Value::Object(map)
}

/// Dispatches one action and returns its envelope.
fn call(action: &str, params: Value) -> Value {
    block_on(router().on_request(Req, payload(action, params)))[action].clone()
}

#[test]
fn camel_case_wire_call_succeeds_and_response_is_camel_case() {
    // Action key is now camelCase: `createUser`, not `create_user`.
    let env = call(
        "createUser",
        json!({ "userName": "alice", "lastLoginAt": 42 }),
    );
    assert_eq!(env["status"], "ok");
    // response serialized camelCase, through the generic `Message<User>`
    assert_eq!(env["data"]["isSuccess"], true);
    assert_eq!(env["data"]["data"]["displayName"], "alice");
    assert!(
        env["data"].get("is_success").is_none(),
        "wire must not carry snake_case: {env}"
    );
}

#[test]
fn snake_case_wire_params_are_rejected() {
    // Under `rename_all = "camelCase"`, the camelCase fields are missing → the
    // payload fails to deserialize (400), proving the rename is active on the wire.
    // Action key is camelCase under the `camel` feature.
    let env = call(
        "createUser",
        json!({ "user_name": "alice", "last_login_at": 42 }),
    );
    assert_eq!(env["status"], "error");
    assert_eq!(env["error"]["code"], 400);
}

#[test]
fn validation_error_path_is_camel_case() {
    // `user_name` shorter than `min_len = 3` → 422, with the field keyed by its
    // camelCase wire name so it lines up with the TypeScript the client holds.
    // Action key is camelCase under the `camel` feature.
    let env = call("createUser", json!({ "userName": "ab", "lastLoginAt": 1 }));
    assert_eq!(env["error"]["code"], 422);
    assert!(
        env["error"]["fields"]["userName"].is_array(),
        "expected camelCase field key: {env}"
    );
    assert!(
        env["error"]["fields"].get("user_name").is_none(),
        "no snake_case field key: {env}"
    );
}

#[test]
fn action_to_action_call_validates_under_camel() {
    // Action keys are camelCase under the `camel` feature.
    let ok = call(
        "registerUser",
        json!({ "userName": "alice", "lastLoginAt": 7 }),
    );
    assert_eq!(ok["status"], "ok");
    assert_eq!(ok["data"]["data"]["displayName"], "alice");

    // the nested `cx.call(create_user, …)` re-validates → 422 on a short name
    let bad = call(
        "registerUser",
        json!({ "userName": "ab", "lastLoginAt": 7 }),
    );
    assert_eq!(bad["error"]["code"], 422);
}

#[test]
fn enum_return_payload_is_camel_case_through_the_router() {
    // Action key is camelCase under the `camel` feature.
    let env = call("announce", json!({ "userName": "ada", "lastLoginAt": 1 }));
    assert_eq!(env["status"], "ok");
    // externally tagged: PascalCase variant tag, camelCase payload field
    assert_eq!(env["data"]["Welcome"]["userName"], "ada");
    assert!(
        env["data"]["Welcome"].get("user_name").is_none(),
        "wire must not carry snake_case: {env}"
    );
}

#[test]
fn nested_dive_validation_path_is_camel_case() {
    // `item_name` shorter than `min_len = 3`, one level down through a `dive`d Vec.
    // Action key is camelCase under the `camel` feature.
    let env = call("checkout", json!({ "lineItems": [{ "itemName": "ab" }] }));
    assert_eq!(env["error"]["code"], 422);
    let fields = env["error"]["fields"].as_object().expect("fields map");
    // every path segment is camelCased — the dived field and the leaf
    assert!(
        fields
            .keys()
            .any(|key| key.contains("lineItems") && key.contains("itemName")),
        "expected camelCase nested path: {fields:?}"
    );
    assert!(
        !fields
            .keys()
            .any(|key| key.contains("line_items") || key.contains("item_name")),
        "no snake_case path segment: {fields:?}"
    );
}

#[test]
fn generated_typescript_uses_camel_case_field_names() {
    let ts = router().generate_ts();

    // field names on a plain model
    assert!(ts.contains("userName: string"), "{ts}");
    assert!(ts.contains("lastLoginAt: number"), "{ts}");
    // ...and through the monomorphized generic `Message<User>`
    assert!(ts.contains("export interface MessageUser {"), "{ts}");
    assert!(ts.contains("isSuccess: boolean"), "{ts}");
    assert!(ts.contains("displayName: string"), "{ts}");

    // Action keys in the interface maps are now camelCase too.
    assert!(
        ts.contains("createUser:"),
        "expected `createUser:` as action key: {ts}"
    );
    assert!(
        ts.contains("registerUser:"),
        "expected `registerUser:` as action key: {ts}"
    );
    assert!(
        ts.contains("announce:"),
        "expected `announce:` as action key: {ts}"
    );
    assert!(
        ts.contains("checkout:"),
        "expected `checkout:` as action key: {ts}"
    );

    // never the original snake_case spellings (fields or action keys)
    for snake in [
        "user_name",
        "last_login_at",
        "is_success",
        "display_name",
        "create_user:",
        "register_user:",
    ] {
        assert!(!ts.contains(snake), "leaked snake_case `{snake}`: {ts}");
    }
}
