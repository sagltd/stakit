//! End-to-end coverage of the v1 garde rule set through `#[derive(Model)]`.
// `&()` ctx is required by garde's custom-validator signature; the case table is
// intentionally a slice of `(name, fn)` pairs.
#![allow(dead_code, clippy::trivially_copy_pass_by_ref, clippy::type_complexity)]

use stakit_model::{Model, garde};

fn non_empty(value: &str, _ctx: &()) -> Result<(), garde::Error> {
    if value.trim().is_empty() {
        Err(garde::Error::new("must not be blank"))
    } else {
        Ok(())
    }
}

#[derive(Model)]
struct Item {
    #[garde(ascii)]
    ascii: String,
    #[garde(alphanumeric)]
    alnum: String,
    #[garde(url)]
    website: String,
    #[garde(contains("foo"))]
    has_foo: String,
    #[garde(prefix("pre"))]
    starts: String,
    #[garde(suffix("post"))]
    ends: String,
    #[garde(pattern("^[0-9]+$"))]
    digits: String,
    #[garde(length(min = 2, max = 4))]
    code: String,
    #[garde(range(min = 1, max = 10))]
    qty: i32,
    #[garde(custom(non_empty))]
    label: String,
    #[garde(skip)]
    anything: String,
}

fn valid() -> Item {
    Item {
        ascii: "abc".into(),
        alnum: "abc123".into(),
        website: "https://example.com".into(),
        has_foo: "xfooy".into(),
        starts: "prefix".into(),
        ends: "the-post".into(),
        digits: "12345".into(),
        code: "abc".into(),
        qty: 5,
        label: "ok".into(),
        anything: String::new(),
    }
}

#[test]
fn fully_valid_item_passes() {
    assert!(valid().validate_model().is_ok());
}

#[test]
fn each_rule_rejects_bad_input() {
    let cases: &[(&str, fn(&mut Item))] = &[
        ("ascii", |i| i.ascii = "café".into()),
        ("alnum", |i| i.alnum = "no spaces!".into()),
        ("website", |i| i.website = "not a url".into()),
        ("has_foo", |i| i.has_foo = "bar".into()),
        ("starts", |i| i.starts = "nope".into()),
        ("ends", |i| i.ends = "nope".into()),
        ("digits", |i| i.digits = "12a".into()),
        ("code", |i| i.code = "x".into()),
        ("qty", |i| i.qty = 99),
        ("label", |i| i.label = "   ".into()),
    ];
    for (field, mutate) in cases {
        let mut item = valid();
        mutate(&mut item);
        let err = item.validate_model().unwrap_err().to_string();
        assert!(
            err.contains(field),
            "expected `{field}` in error, got: {err}"
        );
    }
}

#[test]
fn skip_field_is_never_validated() {
    let mut item = valid();
    item.anything = "literally anything ☃".into();
    assert!(item.validate_model().is_ok());
}

// --- nested validation via `dive` ---

#[derive(Model)]
struct Tag {
    #[garde(length(min = 1))]
    name: String,
}

#[derive(Model)]
struct Post {
    #[garde(dive)]
    tags: Vec<Tag>,
}

#[test]
fn dive_validates_nested_elements() {
    let ok = Post {
        tags: vec![Tag {
            name: "rust".into(),
        }],
    };
    assert!(ok.validate_model().is_ok());

    let bad = Post {
        tags: vec![Tag {
            name: String::new(),
        }],
    };
    assert!(bad.validate_model().is_err());
}
