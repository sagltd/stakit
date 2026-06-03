//! Unit tests for the rule functions and cascading container validation.

use std::collections::HashMap;

use super::{
    Regex, Validate, ValidationErrors, alphanumeric, ascii, contains, email, length, pattern,
    prefix, range, suffix, url,
};

#[test]
fn length_checks_char_bounds() {
    assert!(length("ab", Some(2), Some(4)).is_ok());
    assert!(length("a", Some(2), None).is_err());
    assert!(length("abcde", None, Some(4)).is_err());
}

#[test]
fn ascii_rejects_non_ascii() {
    assert!(ascii("abc").is_ok());
    assert!(ascii("café").is_err());
}

#[test]
fn alphanumeric_rejects_symbols() {
    assert!(alphanumeric("abc123").is_ok());
    assert!(alphanumeric("a b!").is_err());
}

#[test]
fn substring_rules() {
    assert!(contains("xfooy", "foo").is_ok());
    assert!(contains("bar", "foo").is_err());
    assert!(prefix("prefix", "pre").is_ok());
    assert!(suffix("the-post", "post").is_ok());
}

#[test]
fn range_checks_bounds() {
    assert!(range(&5, Some(1), Some(10)).is_ok());
    assert!(range(&0, Some(1), None).is_err());
    assert!(range(&11, None, Some(10)).is_err());
}

#[test]
fn email_structure() {
    assert!(email("a@b.com").is_ok());
    assert!(email("nope").is_err());
    assert!(email("a@b").is_err());
}

#[test]
fn email_rejects_whitespace_and_control_chars() {
    // Spaces anywhere are invalid (the bug: these used to pass).
    assert!(email("samuel @example.com").is_err(), "space in local");
    assert!(email("a@ex ample.com").is_err(), "space in domain");
    assert!(email(" a@b.com").is_err(), "leading space");
    assert!(email("a@b.com ").is_err(), "trailing space");
    assert!(email("a b@c.com").is_err(), "internal space");
    // Tabs / newlines / CR — a newline is an email-header-injection vector.
    assert!(email("a@b.com\n").is_err(), "trailing newline");
    assert!(email("a@b.com\r\nBcc: x@y.com").is_err(), "header injection");
    assert!(email("a\t@b.com").is_err(), "tab");
    // Unicode non-breaking space.
    assert!(email("a\u{00A0}b@c.com").is_err(), "nbsp");
}

#[test]
fn email_rejects_malformed_domains() {
    assert!(email("a@.com").is_err(), "leading dot");
    assert!(email("a@b.com.").is_err(), "trailing dot");
    assert!(email("a@b..com").is_err(), "empty label");
    assert!(email("a@@b.com").is_err(), "double at");
    // Still accepts ordinary multi-label domains.
    assert!(email("a@mail.example.co.uk").is_ok());
}

#[test]
fn url_structure() {
    assert!(url("https://example.com/path").is_ok());
    assert!(url("not a url").is_err());
}

#[test]
fn url_rejects_whitespace_and_control_chars() {
    assert!(url("https://exa mple.com").is_err(), "space in host");
    assert!(url("https://example.com/a b").is_err(), "space in path");
    assert!(url("https://example.com\n").is_err(), "trailing newline");
    assert!(url(" https://example.com").is_err(), "leading space");
    assert!(url("https://example.com\r\nHost: evil").is_err(), "injection");
}

#[test]
fn pattern_matches() {
    let re = Regex::new("^[0-9]+$").unwrap();
    assert!(pattern("123", &re).is_ok());
    assert!(pattern("12a", &re).is_err());
}

// --- cascade / path building ---

struct Inner {
    n: u8,
}

impl Validate for Inner {
    fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errors = ValidationErrors::new();
        if let Err(e) = range(&self.n, Some(1), None) {
            errors.push(e.at_field("n"));
        }
        errors.into_result()
    }
}

#[test]
fn vec_prefixes_index() {
    let items = vec![Inner { n: 1 }, Inner { n: 0 }];
    let err = items.validate().unwrap_err();
    assert_eq!(err.len(), 1);
    assert_eq!(err.iter().next().unwrap().path, "[1].n");
}

#[test]
fn nested_vec_of_map_builds_deep_path() {
    let mut map = HashMap::new();
    map.insert("home".to_string(), Inner { n: 0 });
    let nested = vec![map];
    let err = nested.validate().unwrap_err();
    assert_eq!(err.iter().next().unwrap().path, "[0][home].n");
}

#[test]
fn tuple_validates_each_element() {
    let ok = (Inner { n: 1 }, Inner { n: 2 });
    assert!(ok.validate().is_ok());

    let bad = (Inner { n: 1 }, Inner { n: 0 });
    let err = bad.validate().unwrap_err();
    assert_eq!(err.iter().next().unwrap().path, "[1].n");
}

#[test]
fn option_is_transparent() {
    assert!(Some(Inner { n: 5 }).validate().is_ok());
    assert!(None::<Inner>.validate().is_ok());
    assert!(Some(Inner { n: 0 }).validate().is_err());
}
