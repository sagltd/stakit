use super::{encode, field_value, parse};
use crate::value::{Value, ValueKind};

#[test]
fn encode_simple_scalars() {
    let sql = encode(&[
        Value::Text("Main St".to_owned()),
        Value::Text("NYC".to_owned()),
        Value::Bool(true),
    ]);
    // "Main St" has a space -> quoted; NYC is bare; bool -> t.
    assert_eq!(sql, r#"("Main St",NYC,t)"#);
}

#[test]
fn encode_escapes_quotes_commas_backslash_and_empty() {
    let sql = encode(&[
        Value::Text("a,b".to_owned()),        // comma -> quoted
        Value::Text("say \"hi\"".to_owned()), // quote -> doubled + quoted (space too)
        Value::Text("c\\d".to_owned()),       // backslash -> doubled + quoted
        Value::Text(String::new()),           // empty string -> "" (quoted)
        Value::Null(ValueKind::Text),         // NULL -> empty unquoted
    ]);
    assert_eq!(sql, r#"("a,b","say ""hi""","c\\d","",)"#);
}

#[test]
fn parse_roundtrips_encoded() {
    let original = vec![
        Value::Text("123 Main, Apt 4".to_owned()),
        Value::Text("New York".to_owned()),
        Value::Text("10001".to_owned()),
        Value::Bool(true),
    ];
    let sql = encode(&original);
    let parts = parse(&sql, 4).unwrap();
    assert_eq!(parts[0].as_deref(), Some("123 Main, Apt 4"));
    assert_eq!(parts[1].as_deref(), Some("New York"));
    assert_eq!(parts[2].as_deref(), Some("10001"));
    assert_eq!(parts[3].as_deref(), Some("t"));
}

#[test]
fn parse_distinguishes_null_from_empty_string() {
    let parts = parse(r#"(,"")"#, 2).unwrap();
    assert_eq!(parts[0], None, "unquoted empty = NULL");
    assert_eq!(parts[1].as_deref(), Some(""), "quoted empty = empty string");
}

#[test]
fn parse_unescapes_doubled_quotes_and_backslash() {
    let parts = parse(r#"("say ""hi""","c\\d")"#, 2).unwrap();
    assert_eq!(parts[0].as_deref(), Some(r#"say "hi""#));
    assert_eq!(parts[1].as_deref(), Some(r"c\d"));
}

#[test]
fn parse_rejects_missing_parens_and_bad_arity() {
    assert!(parse("a,b,c", 3).is_err());
    assert!(parse("(a,b)", 3).is_err());
}

#[test]
fn field_value_parses_typed_scalars_and_null() {
    assert_eq!(
        field_value(&Some("42".to_owned()), ValueKind::I32).unwrap(),
        Value::I32(42)
    );
    assert_eq!(
        field_value(&Some("t".to_owned()), ValueKind::Bool).unwrap(),
        Value::Bool(true)
    );
    assert_eq!(
        field_value(&None, ValueKind::Text).unwrap(),
        Value::Null(ValueKind::Text)
    );
    assert!(field_value(&Some("nope".to_owned()), ValueKind::I32).is_err());
}
