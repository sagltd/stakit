//! Unit tests for the built-in [`TSType`] impls.

use std::collections::{BTreeMap, HashMap};

use crate::TSType;

#[test]
fn integers_are_number() {
    assert_eq!(i32::to_ts(), "number");
    assert_eq!(u64::to_ts(), "number");
    assert_eq!(usize::to_ts(), "number");
}

#[test]
fn floats_are_number() {
    assert_eq!(f32::to_ts(), "number");
    assert_eq!(f64::to_ts(), "number");
}

#[test]
fn bool_is_boolean() {
    assert_eq!(bool::to_ts(), "boolean");
}

#[test]
fn string_and_str_are_string() {
    assert_eq!(String::to_ts(), "string");
    assert_eq!(<&str>::to_ts(), "string");
}

#[test]
fn vec_is_array_of_inner() {
    assert_eq!(Vec::<i32>::to_ts(), "Array<number>");
    assert_eq!(Vec::<String>::to_ts(), "Array<string>");
}

#[test]
fn option_is_union_with_undefined() {
    assert_eq!(Option::<String>::to_ts(), "string | undefined");
}

#[test]
fn nested_generics_compose() {
    assert_eq!(Vec::<Option<i32>>::to_ts(), "Array<number | undefined>");
}

#[test]
fn hashmap_is_record() {
    assert_eq!(HashMap::<String, i32>::to_ts(), "Record<string, number>");
    assert_eq!(BTreeMap::<String, bool>::to_ts(), "Record<string, boolean>");
}

#[test]
fn hashbrown_indexmap_are_record() {
    assert_eq!(
        hashbrown::HashMap::<String, i32>::to_ts(),
        "Record<string, number>"
    );
    assert_eq!(
        indexmap::IndexMap::<String, i32>::to_ts(),
        "Record<string, number>"
    );
}

#[test]
fn array_is_array_of_inner() {
    assert_eq!(<[u8; 4]>::to_ts(), "Array<number>");
}

#[test]
fn tuple_is_positional() {
    assert_eq!(<(i32, String)>::to_ts(), "[number, string]");
}
