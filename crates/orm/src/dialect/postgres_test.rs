use super::PostgresDialect;
use crate::dialect::Dialect;

#[test]
fn uses_dollar_numbered_placeholders() {
    let dialect = PostgresDialect;
    assert_eq!(dialect.placeholder_prefix(), '$');
    assert!(dialect.numbered_placeholders());
}

#[test]
fn supports_any_array() {
    assert!(PostgresDialect.supports_any_array());
}

#[test]
fn name_is_postgres() {
    assert_eq!(PostgresDialect.name(), "postgres");
}
