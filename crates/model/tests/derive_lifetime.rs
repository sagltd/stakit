//! Battle-tests for `#[derive(Model)]` on types with lifetime and generic
//! parameters — the zero-copy use case (`email: &'a str` borrowed straight from
//! a request buffer) plus generic models that themselves implement [`Model`].
#![allow(dead_code)]

use std::borrow::Cow;

use stakit_model::{Model, TSType, Validate, generate_typescript};

// --- single lifetime, borrowed `&str` field (the zero-copy headline case) ---

#[derive(Model)]
struct LoginUserParams<'a> {
    /// Email or phone identifying the account.
    #[validate(min_len = 1, max_len = 320, email)]
    email: &'a str,
}

#[test]
fn borrowed_str_validates_without_owning() {
    let raw = String::from(r#"{"email":"a@b.com"}"#);
    // `email` borrows out of `raw` — no allocation, no copy.
    let params = LoginUserParams {
        email: &raw[10..17],
    };
    assert_eq!(params.email, "a@b.com");
    assert!(params.validate().is_ok());
}

#[test]
fn borrowed_str_field_fails_bad_email() {
    let params = LoginUserParams { email: "nope" };
    let err = params.validate().unwrap_err();
    assert!(err.to_string().contains("email"), "{err}");
}

#[test]
fn borrowed_str_renders_typescript() {
    let ts = generate_typescript::<LoginUserParams<'_>>();
    assert!(ts.contains("export interface LoginUserParams {"), "{ts}");
    assert!(ts.contains("email: string;"), "{ts}");
}

// --- multiple lifetimes + borrowed slice ---

#[derive(Model)]
struct MultiBorrow<'a, 'b> {
    #[validate(min_len = 1)]
    name: &'a str,
    tags: &'b [&'a str],
}

#[test]
fn multiple_lifetimes_compile_and_validate() {
    let tags = ["x", "y"];
    let m = MultiBorrow {
        name: "ok",
        tags: &tags,
    };
    assert!(m.validate().is_ok());
}

// --- Cow: ambiguous-ownership borrowed-or-owned ---

#[derive(Model)]
struct WithCow<'a> {
    #[validate(min_len = 2)]
    label: Cow<'a, str>,
}

#[test]
fn cow_field_validates_and_renders() {
    let borrowed = WithCow {
        label: Cow::Borrowed("hi"),
    };
    assert!(borrowed.validate().is_ok());
    let ts = generate_typescript::<WithCow<'_>>();
    assert!(ts.contains("label: string;"), "{ts}");
}

// --- generic type param that is itself a Model (dive) ---

#[derive(Model)]
struct Inner {
    #[validate(min_len = 1)]
    v: String,
}

#[derive(Model)]
struct Envelope<T>
where
    T: Model,
{
    #[validate(dive)]
    payload: T,
    #[validate(min = 0)]
    seq: u32,
}

#[test]
fn generic_model_dives_into_payload() {
    let good = Envelope {
        payload: Inner { v: "x".into() },
        seq: 1,
    };
    assert!(good.validate().is_ok());

    let bad = Envelope {
        payload: Inner { v: String::new() },
        seq: 1,
    };
    let err = bad.validate().unwrap_err();
    assert!(err.to_string().contains("payload"), "{err}");
}

#[test]
fn generic_model_monomorphizes_ts_name() {
    let ts = generate_typescript::<Envelope<Inner>>();
    // `ts_ref` for a generic instantiation appends the arg's name.
    assert!(ts.contains("export interface EnvelopeInner {"), "{ts}");
    assert!(ts.contains("payload: Inner;"), "{ts}");
}

// --- lifetime AND generic together ---

#[derive(Model)]
struct Tagged<'a, T>
where
    T: Model,
{
    #[validate(min_len = 1)]
    kind: &'a str,
    #[validate(dive)]
    inner: T,
}

#[test]
fn lifetime_plus_generic_compiles() {
    let t = Tagged {
        kind: "user",
        inner: Inner { v: "y".into() },
    };
    assert!(t.validate().is_ok());
    // Both the lifetime and the generic survive into the TSType impl.
    let _ = <Tagged<'_, Inner> as TSType>::ts_ref();
}
