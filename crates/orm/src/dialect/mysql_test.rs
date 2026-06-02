use super::MySqlDialect;
use crate::dialect::Dialect;

#[test]
fn uses_bare_question_placeholders() {
    let dialect = MySqlDialect;
    assert_eq!(dialect.placeholder_prefix(), '?');
    assert!(!dialect.numbered_placeholders());
}

#[test]
fn no_array_membership() {
    assert!(!MySqlDialect.supports_any_array());
}

#[test]
fn name_is_mysql() {
    assert_eq!(MySqlDialect.name(), "mysql");
}

#[test]
fn no_returning_support() {
    assert!(!MySqlDialect.supports_returning());
}

#[test]
fn renders_backtick_idents_and_bare_placeholders() {
    // Offline proof that the builder emits valid MySQL SQL: backtick-quoted
    // identifiers and bare `?` placeholders (no live server needed).
    use crate::sql::SqlWriter;
    use crate::value::Value;
    let mut writer = SqlWriter::with_dialect(&MySqlDialect);
    writer.push_qualified("users", "id").unwrap();
    writer.push(" = ");
    writer.push_bind(Value::I64(1));
    writer.push(" and ");
    writer.push_ident("name").unwrap();
    writer.push(" = ");
    writer.push_bind(Value::Text("x".to_owned()));
    assert_eq!(writer.sql(), "`users`.`id` = ? and `name` = ?");
}
