//! End-to-end tests for `#[derive(Model)]` on structs.
#![allow(dead_code)]

use stakit_model::{Model, Validate, generate_typescript};

#[derive(Model)]
struct User {
    #[validate(min_len = 3, max_len = 20)]
    name: String,
    #[validate(email)]
    email: String,
    #[validate(min = 18)]
    age: u8,
    bio: Option<String>,
}

#[test]
fn ts_interface_renders_each_field() {
    let ts = generate_typescript::<User>();
    assert!(ts.contains("export interface User {"), "{ts}");
    assert!(ts.contains("name: string;"), "{ts}");
    assert!(ts.contains("email: string;"), "{ts}");
    assert!(ts.contains("age: number;"), "{ts}");
    assert!(ts.contains("bio?: string;"), "{ts}");
}

#[test]
fn valid_user_passes_validation() {
    let user = User {
        name: "bob".into(),
        email: "bob@example.com".into(),
        age: 20,
        bio: None,
    };
    assert!(user.validate().is_ok());
}

#[test]
fn short_name_fails_validation() {
    let user = User {
        name: "x".into(),
        email: "bob@example.com".into(),
        age: 20,
        bio: None,
    };
    let err = user.validate().unwrap_err();
    assert!(err.to_string().contains("name"), "{err}");
}

#[test]
fn bad_email_fails_validation() {
    let user = User {
        name: "bob".into(),
        email: "not-an-email".into(),
        age: 20,
        bio: None,
    };
    let err = user.validate().unwrap_err();
    assert!(err.to_string().contains("email"), "{err}");
}

#[test]
fn underage_fails_validation() {
    let user = User {
        name: "bob".into(),
        email: "bob@example.com".into(),
        age: 5,
        bio: None,
    };
    let err = user.validate().unwrap_err();
    assert!(err.to_string().contains("age"), "{err}");
}

#[test]
fn multiple_failures_are_all_collected() {
    let user = User {
        name: "x".into(),
        email: "nope".into(),
        age: 5,
        bio: None,
    };
    let err = user.validate().unwrap_err();
    assert_eq!(err.len(), 3, "{err}");
    let fields = err.field_errors();
    assert!(fields.contains_key("name"));
    assert!(fields.contains_key("email"));
    assert!(fields.contains_key("age"));
}
