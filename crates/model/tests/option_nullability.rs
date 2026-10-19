//! TypeScript nullability follows actual serde JSON, including nested values.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::json;
use stakit_model::{Model, TSType, generate_typescript, model};

#[model]
#[derive(Default)]
struct OptionalRow {
    plain: Option<String>,
    #[serde(default)]
    defaulted: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skipped: Option<String>,
    values: Vec<Option<i32>>,
    lookup: BTreeMap<String, Option<bool>>,
}

#[derive(Model, Serialize, Deserialize)]
struct NullableTuple(Option<String>, Vec<Option<i32>>);

#[derive(Model, Serialize, Deserialize)]
#[serde(untagged)]
enum NullableValue {
    Text(Option<String>),
    Pair(Option<String>, Option<i32>),
    Record { value: Option<bool> },
}

#[model]
#[serde(tag = "kind")]
enum NullableFrame {
    Reply { source: Option<String> },
}

#[test]
fn an_option_value_uses_json_null_instead_of_undefined() {
    assert_eq!(serde_json::to_value(None::<String>).unwrap(), json!(null));
    assert_eq!(Option::<String>::ts_ref(), "string | null");
    assert_eq!(Option::<Option<String>>::ts_ref(), "string | null | null");
    assert_eq!(
        serde_json::to_value(Some(None::<String>)).unwrap(),
        json!(null)
    );
    assert_eq!(
        Option::<Vec<Option<String>>>::ts_ref(),
        "Array<string | null> | null"
    );
    assert_eq!(
        <(Option<String>, Option<Vec<i32>>)>::ts_ref(),
        "[string | null, Array<number> | null]"
    );
}

#[test]
fn optional_properties_accept_missing_null_and_present_values() {
    let missing: OptionalRow = serde_json::from_value(json!({"values":[],"lookup":{}})).unwrap();
    assert_eq!(
        serde_json::to_value(missing).unwrap(),
        json!({"plain":null,"defaulted":null,"values":[],"lookup":{}})
    );
    let explicit: OptionalRow = serde_json::from_value(json!({
        "plain":null,"defaulted":null,"skipped":null,
        "values":[null,2],"lookup":{"yes":true,"missing":null}
    }))
    .unwrap();
    assert_eq!(
        serde_json::to_value(explicit).unwrap(),
        json!({
            "plain":null,"defaulted":null,
            "values":[null,2],"lookup":{"yes":true,"missing":null}
        })
    );
    let present: OptionalRow = serde_json::from_value(json!({
        "plain":"p","defaulted":"d","skipped":"s","values":[],"lookup":{}
    }))
    .unwrap();
    assert_eq!(present.skipped.as_deref(), Some("s"));
    assert_eq!(
        generate_typescript::<OptionalRow>(),
        "export interface OptionalRow {\n  plain?: string | null;\n  defaulted?: string | null;\n  skipped?: string | null;\n  values: Array<number | null>;\n  lookup: Record<string, boolean | null>;\n}"
    );
}

#[test]
fn tuple_positions_preserve_nullable_values_instead_of_stripping_the_option() {
    let value = NullableTuple(None, vec![None, Some(3)]);
    assert_eq!(
        serde_json::to_value(value).unwrap(),
        json!([null, [null, 3]])
    );
    assert_eq!(
        generate_typescript::<NullableTuple>(),
        "export type NullableTuple = [string | null, Array<number | null>];"
    );
}

#[test]
fn nullable_enum_payloads_and_named_variant_fields_match_serde() {
    assert_eq!(
        serde_json::to_value(NullableValue::Text(None)).unwrap(),
        json!(null)
    );
    assert_eq!(
        serde_json::to_value(NullableValue::Pair(None, Some(4))).unwrap(),
        json!([null, 4])
    );
    assert_eq!(
        serde_json::to_value(NullableValue::Record { value: None }).unwrap(),
        json!({"value":null})
    );
    assert_eq!(
        generate_typescript::<NullableValue>(),
        "export type NullableValue = string | null | [string | null, number | null] | { value?: boolean | null };"
    );
    assert_eq!(
        serde_json::to_value(NullableFrame::Reply { source: None }).unwrap(),
        json!({"kind":"Reply","source":null})
    );
    assert_eq!(
        generate_typescript::<NullableFrame>(),
        "export type NullableFrame = { kind: \"Reply\"; source?: string | null };"
    );
}

#[test]
fn transitive_declarations_survive_nullable_named_models() {
    #[model]
    struct Source {
        id: String,
    }
    #[model]
    struct Response {
        source: Option<Source>,
    }

    let ts = generate_typescript::<Response>();
    assert!(ts.contains("source?: Source | null;"), "{ts}");
    assert!(
        ts.contains("export interface Source {\n  id: string;\n}"),
        "{ts}"
    );
}
