//! Verifies the `camel` feature: `snake_case` Rust fields become `camelCase` in
//! the generated TypeScript and validation paths, aligning with a single serde
//! `#[serde(rename_all = "camelCase")]` line for the wire format.
//!
//! Run: `cargo test -p stakit-model --features camel`.
#![cfg(feature = "camel")]
#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use stakit_model::{Model, Validate, generate_typescript};

#[derive(Model, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Account {
    #[validate(min_len = 3)]
    user_name: String,
    last_login_at: u64,
}

#[test]
fn typescript_fields_are_camel_case() {
    let ts = generate_typescript::<Account>();
    assert!(ts.contains("userName: string"), "{ts}");
    assert!(ts.contains("lastLoginAt: number"), "{ts}");
    assert!(!ts.contains("user_name"), "{ts}");
}

#[test]
fn serde_wire_is_camel_case_with_one_line() {
    let account = Account {
        user_name: "bob".to_owned(),
        last_login_at: 42,
    };
    let json = serde_json::to_string(&account).unwrap();
    assert_eq!(json, r#"{"userName":"bob","lastLoginAt":42}"#);

    let back: Account = serde_json::from_str(r#"{"userName":"ada","lastLoginAt":7}"#).unwrap();
    assert_eq!(back.user_name, "ada");
}

#[test]
fn validation_path_is_camel_case() {
    let account = Account {
        user_name: "x".to_owned(),
        last_login_at: 0,
    };
    let err = account.validate().unwrap_err();
    assert_eq!(err.iter().next().unwrap().path, "userName");
}
