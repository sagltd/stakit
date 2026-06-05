//! Verifies the `camel` feature: `snake_case` Rust fields become `camelCase` in
//! the generated TypeScript and validation paths, aligning with a single serde
//! `#[serde(rename_all = "camelCase")]` line for the wire format.
//!
//! Run: `cargo test -p stakit-model --features camel`.
#![cfg(feature = "camel")]
#![allow(dead_code)]
// A leading-underscore field (`_secret`) is a deliberate test subject: it is the
// case where a naive snake→camel pass diverges from serde's rule.
#![allow(clippy::used_underscore_binding)]

use serde::{Deserialize, Serialize};
use stakit_model::{Model, Validate, generate_typescript, model};

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

// serde owns the wire name; `wire_name` must reproduce its `RenameRule::CamelCase`
// exactly, including for identifiers where a naive snake→camel pass diverges — a
// leading underscore, which serde folds into the next word and then lowercases.
#[model]
struct Edgy {
    #[validate(min_len = 1)]
    _secret: String,
    created_at: u64,
}

#[test]
fn wire_name_matches_serde_for_a_leading_underscore_field() {
    // serde renames `_secret` → `secret` (PascalCase "Secret", first char lowered),
    // NOT "Secret"; the TS + validation path must agree byte-for-byte.
    let wire = serde_json::to_value(Edgy {
        _secret: "x".to_owned(),
        created_at: 1,
    })
    .unwrap();
    let keys: Vec<&str> = wire
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert!(keys.contains(&"secret"), "serde wire keys: {keys:?}");
    assert!(!keys.contains(&"Secret"), "serde wire keys: {keys:?}");

    let ts = generate_typescript::<Edgy>();
    assert!(ts.contains("secret: string"), "{ts}");
    assert!(!ts.contains("Secret"), "{ts}");
    assert!(ts.contains("createdAt: number"), "{ts}");

    let err = Edgy {
        _secret: String::new(),
        created_at: 1,
    }
    .validate()
    .unwrap_err();
    assert_eq!(err.iter().next().unwrap().path, "secret");
}

// Enums: `#[model]` injects `rename_all_fields` (not `rename_all`), so a struct
// variant's payload fields are camelCase on the wire (matching the TS + the
// validation path) while the variant tag stays verbatim PascalCase.
#[model]
enum Event {
    UserCreated {
        #[validate(min_len = 3)]
        user_name: String,
        last_login_at: u64,
    },
    Ping,
}

#[test]
fn enum_struct_variant_fields_are_camel_case_on_wire_and_in_typescript() {
    let wire = serde_json::to_value(Event::UserCreated {
        user_name: "bob".to_owned(),
        last_login_at: 7,
    })
    .unwrap();
    // externally tagged: PascalCase variant tag, camelCase payload fields
    let payload = &wire["UserCreated"];
    assert!(payload.get("userName").is_some(), "{wire}");
    assert!(payload.get("user_name").is_none(), "{wire}");
    assert_eq!(payload["lastLoginAt"], 7);

    let ts = generate_typescript::<Event>();
    assert!(ts.contains("userName: string"), "{ts}");
    assert!(ts.contains("lastLoginAt: number"), "{ts}");
    assert!(!ts.contains("user_name"), "{ts}");
    assert!(ts.contains("\"Ping\""), "{ts}"); // unit tag verbatim, matches the wire
}

#[test]
fn enum_validation_path_is_camel_case() {
    let err = Event::UserCreated {
        user_name: "ab".to_owned(),
        last_login_at: 0,
    }
    .validate()
    .unwrap_err();
    assert_eq!(err.iter().next().unwrap().path, "userName");
}
