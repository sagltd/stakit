use crate::dialect::SqliteDialect;
use crate::sql::SqlWriter;

#[test]
fn qualified_column_is_quoted() {
    let mut writer = SqlWriter::new();
    writer.push_qualified("users", "id").unwrap();
    assert_eq!(writer.sql(), r#""users"."id""#);
}

#[test]
fn sqlite_dialect_uses_question_placeholders() {
    let mut writer = SqlWriter::with_dialect(&SqliteDialect);
    writer.push("a = ");
    writer.push_bind(crate::value::Value::I32(1));
    writer.push(" and b = ");
    writer.push_bind(crate::value::Value::I32(2));
    assert_eq!(writer.sql(), "a = ?1 and b = ?2");
}

#[test]
fn binds_are_numbered_in_order() {
    let mut writer = SqlWriter::new();
    writer.push("a = ");
    writer.push_bind(crate::value::Value::I32(10));
    writer.push(" and b = ");
    writer.push_bind(crate::value::Value::I32(20));
    assert_eq!(writer.sql(), "a = $1 and b = $2");
    assert_eq!(writer.bind_count(), 2);
}

#[test]
fn itoa_renders_multi_digit() {
    let mut writer = SqlWriter::new();
    for _ in 0..12 {
        writer.push_bind(crate::value::Value::I32(1));
        writer.push(" ");
    }
    assert!(writer.sql().contains("$12"));
}

#[test]
fn itoa_renders_three_digit_placeholder() {
    let mut writer = SqlWriter::new();
    for _ in 0..123 {
        writer.push_bind(crate::value::Value::I32(1));
        writer.push(" ");
    }
    assert!(writer.sql().ends_with("$123 "));
    assert_eq!(writer.bind_count(), 123);
}

#[test]
fn push_ident_quotes_single_identifier() {
    let mut writer = SqlWriter::new();
    writer.push_ident("users").unwrap();
    assert_eq!(writer.sql(), r#""users""#);
}

#[test]
fn push_ident_rejects_empty() {
    let mut writer = SqlWriter::new();
    assert!(writer.push_ident("").is_err());
}

#[test]
fn into_parts_returns_sql_and_binds() {
    let mut writer = SqlWriter::new();
    writer.push("x = ");
    writer.push_bind(crate::value::Value::I32(7));
    let (sql, binds) = writer.into_parts();
    assert_eq!(sql, "x = $1");
    assert_eq!(binds.len(), 1);
}

#[test]
fn default_matches_new() {
    let writer = SqlWriter::default();
    assert_eq!(writer.sql(), "");
    assert_eq!(writer.bind_count(), 0);
}

#[test]
fn first_bind_is_dollar_one() {
    let mut writer = SqlWriter::new();
    writer.push_bind(crate::value::Value::I32(1));
    assert_eq!(writer.sql(), "$1");
}
