//! `#[derive(ResponseError)]`: the idiomatic way to define an action's error
//! type. One enum + thiserror gives per-variant HTTP status, a machine-readable
//! code, `?`-propagation of foreign errors via `#[from]`, and automatic
//! conversion into the router's `Error` (status preserved, 5xx genericized).

#![allow(dead_code)]

use futures::executor::block_on;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use stakit_model::Model;
use stakit_router::{Cx, Error, ResponseError, Router, action};

// A "foreign" error (stand-in for `stakit_orm::Error`): note it does NOT impl
// `ResponseError` — it flows into `ActionError` through thiserror's `#[from]`.
#[derive(Debug, thiserror::Error)]
#[error("connection refused: postgres://admin:hunter2@db")]
struct DbError;

// Stand-in for a real DB lookup that can fail.
fn query_user(name: &str) -> Result<Account, DbError> {
    if name == "ghost" {
        Err(DbError) // simulate the database being unavailable
    } else {
        Ok(Account { id: 1 })
    }
}

#[derive(Model, Serialize, Deserialize)]
struct Account {
    id: u64,
}

// THE WHOLE ERROR TYPE: ~12 lines replaces a hand-written Display + status() +
// `From` + db-mapping helper.
#[derive(Debug, thiserror::Error, ResponseError)]
enum ActionError {
    #[status(404)]
    #[error("user not found")]
    UserNotFound,

    #[status(401)]
    #[code("login_failed")] // explicit override of the default `invalid_credentials`
    #[error("invalid credentials")]
    InvalidCredentials,

    #[status(400)]
    #[error("{0} is required")]
    BadRequest(&'static str),

    // `?` on a `DbError` converts here automatically (thiserror `#[from]`); the
    // 500 message is genericized for the client, real text kept for logging.
    #[status(500)]
    #[error(transparent)]
    Db(#[from] DbError),
}

#[derive(Model, Serialize, Deserialize)]
struct Lookup {
    name: String,
}

struct App;

// Every arm below is just `?` or a bare `Err(...)` — no `.map_err`, no manual
// status. The router converts `ActionError` → `Error` on its own.
#[action]
async fn lookup(_cx: &Cx<App, ()>, params: Lookup) -> Result<Account, ActionError> {
    match params.name.as_str() {
        "missing" => Err(ActionError::UserNotFound),
        "nobody" => Err(ActionError::InvalidCredentials),
        "blank" => Err(ActionError::BadRequest("name")),
        // `?` on the `DbError` auto-converts via the `#[from]` variant.
        name => Ok(query_user(name)?),
    }
}

fn router() -> Router<App, ()> {
    Router::builder().ctx(App).register(lookup).build()
}

fn call(name_value: &Value) -> Value {
    let out = block_on(router().on_request((), json!({ "lookup": name_value })));
    out["lookup"].clone()
}

#[test]
fn not_found_maps_to_404_with_default_code() {
    let env = call(&json!({ "name": "missing" }));
    assert_eq!(env["status"], "error");
    assert_eq!(env["error"]["code"], 404);
    // default machine code = snake_case of the variant name.
    assert_eq!(env["error"]["type"], "user_not_found");
    assert_eq!(env["error"]["message"], "user not found");
}

#[test]
fn custom_machine_code_is_emitted() {
    let env = call(&json!({ "name": "nobody" }));
    assert_eq!(env["error"]["code"], 401);
    // `#[code("login_failed")]` overrides the default `invalid_credentials`.
    assert_eq!(env["error"]["type"], "login_failed");
    assert_eq!(env["error"]["message"], "invalid credentials");
}

#[test]
fn display_args_flow_into_the_client_message() {
    let env = call(&json!({ "name": "blank" }));
    assert_eq!(env["error"]["code"], 400);
    assert_eq!(env["error"]["message"], "name is required");
}

#[test]
fn foreign_db_error_converts_via_from_and_is_genericized() {
    let env = call(&json!({ "name": "ghost" }));
    // status from the `#[status(500)]` on the `#[from]` variant...
    assert_eq!(env["error"]["code"], 500);
    // ...client sees a generic message (the DB URL is NOT leaked)...
    assert_eq!(env["error"]["message"], "internal server error");
    assert!(
        !env.to_string().contains("hunter2"),
        "internal detail leaked: {env}"
    );
}

#[test]
fn from_preserves_status_and_detail_for_logging() {
    // The `From<ActionError>` keeps the real text in `detail` (server-side only).
    let err: Error = ActionError::Db(DbError).into();
    assert_eq!(err.code, 500);
    assert_eq!(err.kind, "db");
    assert_eq!(
        err.detail(),
        Some("connection refused: postgres://admin:hunter2@db")
    );

    // A sub-500 error carries no detail and its real message reaches the client.
    let err: Error = ActionError::UserNotFound.into();
    assert_eq!(err.code, 404);
    assert_eq!(err.message, "user not found");
    assert_eq!(err.detail(), None);
}

#[test]
fn typescript_generates_error_code_union_and_guard() {
    let ts = router().generate_ts();

    // A string union of every code: this action's (default + overridden) plus
    // the router's built-ins.
    assert!(ts.contains("export type ErrorCode ="), "{ts}");
    assert!(ts.contains("\"user_not_found\""), "{ts}"); // default snake_case
    assert!(ts.contains("\"login_failed\""), "{ts}"); // #[code(...)] override
    assert!(ts.contains("\"db\""), "{ts}"); // the #[from] variant's code
    // built-ins are always present so clients can match them exhaustively.
    for builtin in [
        "\"validation\"",
        "\"not_found\"",
        "\"internal\"",
        "\"bad_request\"",
    ] {
        assert!(ts.contains(builtin), "missing built-in {builtin} in: {ts}");
    }

    // the typed envelope references the union, and a narrowing guard is emitted.
    assert!(ts.contains("export interface ResponseError {"), "{ts}");
    assert!(ts.contains("type: ErrorCode;"), "{ts}");
    assert!(ts.contains("export const isValidationError ="), "{ts}");
}
