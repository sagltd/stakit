//! End-to-end: deep nested-container validation via `#[validate(dive)]`.
#![allow(dead_code)]

use std::collections::HashMap;

use stakit_model::{Model, Validate};

#[derive(Model)]
struct Inner {
    #[validate(min = 1)]
    n: u8,
}

#[derive(Model)]
struct Outer {
    #[validate(dive)]
    rows: Vec<HashMap<String, Inner>>,
}

#[test]
fn deep_nested_failure_path() {
    let mut row = HashMap::new();
    row.insert("home".to_string(), Inner { n: 0 });
    let outer = Outer { rows: vec![row] };
    let err = outer.validate().unwrap_err();
    assert_eq!(err.iter().next().unwrap().path, "rows[0][home].n", "{err}");
}

#[test]
fn deep_nested_ok() {
    let mut row = HashMap::new();
    row.insert("home".to_string(), Inner { n: 5 });
    let outer = Outer { rows: vec![row] };
    assert!(outer.validate().is_ok());
}
