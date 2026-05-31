//! End-to-end coverage of the `#[validate(...)]` rule set through `#[derive(Model)]`.
#![allow(dead_code, clippy::type_complexity)]

use stakit_model::{Model, Validate, ValidationError};

fn non_empty(value: &str) -> Result<(), ValidationError> {
    if value.trim().is_empty() {
        Err(ValidationError::new("non_empty", "must not be blank"))
    } else {
        Ok(())
    }
}

#[derive(Model)]
struct Item {
    #[validate(ascii)]
    ascii: String,
    #[validate(alphanumeric)]
    alnum: String,
    #[validate(url)]
    website: String,
    #[validate(contains = "foo")]
    has_foo: String,
    #[validate(prefix = "pre")]
    starts: String,
    #[validate(suffix = "post")]
    ends: String,
    #[validate(pattern = "^[0-9]+$")]
    digits: String,
    #[validate(min_len = 2, max_len = 4)]
    code: String,
    #[validate(min = 1, max = 10)]
    qty: i32,
    #[validate(custom = non_empty)]
    label: String,
    #[validate(skip)]
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
    assert!(valid().validate().is_ok());
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
        let err = item.validate().unwrap_err().to_string();
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
    assert!(item.validate().is_ok());
}

// --- nested validation via `dive` ---

#[derive(Model)]
struct Tag {
    #[validate(min_len = 1)]
    name: String,
}

#[derive(Model)]
struct Post {
    #[validate(dive)]
    tags: Vec<Tag>,
}

#[test]
fn dive_validates_nested_elements() {
    let ok = Post {
        tags: vec![Tag {
            name: "rust".into(),
        }],
    };
    assert!(ok.validate().is_ok());

    let bad = Post {
        tags: vec![Tag {
            name: String::new(),
        }],
    };
    let err = bad.validate().unwrap_err();
    // path should point at the nested element + field
    assert_eq!(err.iter().next().unwrap().path, "tags[0].name", "{err}");
}
