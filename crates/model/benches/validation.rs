//! Divan benchmarks for `stakit-model`: validation hot path + TS generation.
#![allow(dead_code)]

use divan::{Bencher, black_box};
use stakit_model::{Model, Validate, generate_typescript};

fn main() {
    divan::main();
}

#[derive(Model)]
struct User {
    #[validate(min_len = 3, max_len = 20)]
    name: String,
    #[validate(email)]
    email: String,
    #[validate(min = 18, max = 120)]
    age: u8,
    bio: Option<String>,
}

fn valid_user() -> User {
    User {
        name: "alice".to_owned(),
        email: "alice@example.com".to_owned(),
        age: 30,
        bio: None,
    }
}

fn invalid_user() -> User {
    User {
        name: "a".to_owned(),
        email: "not-an-email".to_owned(),
        age: 5,
        bio: None,
    }
}

/// Validating a struct that passes every rule (construction excluded).
#[divan::bench]
fn validate_valid(bencher: Bencher<'_, '_>) {
    bencher
        .with_inputs(valid_user)
        .bench_refs(|user| black_box(user.validate().is_ok()));
}

/// Validating a struct that fails every rule (error aggregation path).
#[divan::bench]
fn validate_invalid(bencher: Bencher<'_, '_>) {
    bencher
        .with_inputs(invalid_user)
        .bench_refs(|user| black_box(user.validate().is_err()));
}

/// Generating the TypeScript interface for a model.
#[divan::bench]
fn generate_ts() {
    black_box(generate_typescript::<User>());
}
