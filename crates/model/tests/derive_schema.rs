//! End-to-end tests for `#[derive(JsonSchema)]` (the `schema` feature).
#![cfg(feature = "schema")]
#![allow(dead_code)]

use stakit_model::{JsonSchema, Model};

#[derive(Model, JsonSchema)]
struct Weather {
    /// City name, e.g. "Paris"
    #[validate(min_len = 2, max_len = 64)]
    city: String,
    /// Temperature unit
    units: Option<String>,
    #[validate(min = 1, max = 14)]
    days: u8,
    tags: Vec<String>,
}

#[test]
fn object_with_typed_properties() {
    let s = Weather::schema();
    assert_eq!(s["type"], "object");
    assert_eq!(s["properties"]["city"]["type"], "string");
    assert_eq!(s["properties"]["units"]["type"], "string");
    assert_eq!(s["properties"]["days"]["type"], "integer");
    assert_eq!(s["properties"]["tags"]["type"], "array");
    assert_eq!(s["properties"]["tags"]["items"]["type"], "string");
}

#[test]
fn string_length_maps_to_minlength_maxlength() {
    let s = Weather::schema();
    assert_eq!(s["properties"]["city"]["minLength"], 2);
    assert_eq!(s["properties"]["city"]["maxLength"], 64);
}

#[test]
fn numeric_range_maps_to_minimum_maximum() {
    let s = Weather::schema();
    assert_eq!(s["properties"]["days"]["minimum"], 1);
    assert_eq!(s["properties"]["days"]["maximum"], 14);
}

#[test]
fn doc_comment_becomes_property_description() {
    let s = Weather::schema();
    assert_eq!(
        s["properties"]["city"]["description"],
        "City name, e.g. \"Paris\""
    );
}

#[test]
fn required_lists_non_option_fields_only() {
    let s = Weather::schema();
    let req: Vec<&str> = s["required"]
        .as_array()
        .expect("required is an array")
        .iter()
        .map(|v| v.as_str().expect("required entries are strings"))
        .collect();
    assert!(req.contains(&"city"), "{req:?}");
    assert!(req.contains(&"days"), "{req:?}");
    assert!(req.contains(&"tags"), "{req:?}");
    assert!(!req.contains(&"units"), "{req:?}");
}

#[derive(JsonSchema)]
struct Explicit {
    /// doc that is overridden
    #[arg(description = "explicit override")]
    field: String,
}

#[test]
fn arg_description_overrides_doc_comment() {
    let s = Explicit::schema();
    assert_eq!(s["properties"]["field"]["description"], "explicit override");
}

#[test]
fn vec_length_maps_to_minitems_maxitems() {
    #[derive(JsonSchema)]
    struct Bag {
        #[validate(min_len = 1, max_len = 5)]
        items: Vec<String>,
    }
    let s = Bag::schema();
    assert_eq!(s["properties"]["items"]["type"], "array");
    assert_eq!(s["properties"]["items"]["minItems"], 1);
    assert_eq!(s["properties"]["items"]["maxItems"], 5);
}

#[derive(JsonSchema)]
enum Unit {
    Celsius,
    Fahrenheit,
}

#[test]
fn all_unit_enum_maps_to_string_enum() {
    let s = Unit::schema();
    assert_eq!(s["type"], "string");
    let variants: Vec<&str> = s["enum"]
        .as_array()
        .expect("enum array")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(variants, vec!["Celsius", "Fahrenheit"]);
}

#[derive(JsonSchema)]
enum Shape {
    Circle { radius: f64 },
    Named(String),
}

#[test]
fn data_enum_maps_to_oneof_externally_tagged() {
    let s = Shape::schema();
    let one_of = s["oneOf"].as_array().expect("oneOf array");
    assert_eq!(one_of.len(), 2);
    assert_eq!(one_of[0]["properties"]["Circle"]["type"], "object");
    assert_eq!(
        one_of[0]["properties"]["Circle"]["properties"]["radius"]["type"],
        "number"
    );
    assert_eq!(one_of[1]["properties"]["Named"]["type"], "string");
}

#[derive(JsonSchema)]
struct Pair(i32, String);

#[test]
fn tuple_struct_maps_to_prefixitems() {
    let s = Pair::schema();
    assert_eq!(s["type"], "array");
    assert_eq!(s["minItems"], 2);
    assert_eq!(s["maxItems"], 2);
    assert_eq!(s["prefixItems"][0]["type"], "integer");
    assert_eq!(s["prefixItems"][1]["type"], "string");
}

#[derive(JsonSchema)]
struct Inner {
    value: i32,
}

#[derive(JsonSchema)]
struct Outer {
    #[validate(dive)]
    inner: Inner,
    flag: bool,
}

#[test]
fn nested_struct_recurses_into_subschema() {
    let s = Outer::schema();
    assert_eq!(s["properties"]["inner"]["type"], "object");
    assert_eq!(
        s["properties"]["inner"]["properties"]["value"]["type"],
        "integer"
    );
    assert_eq!(s["properties"]["flag"]["type"], "boolean");
}
