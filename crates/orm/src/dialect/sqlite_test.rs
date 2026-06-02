use super::SqliteDialect;
use crate::dialect::Dialect;

#[test]
fn uses_question_numbered_placeholders() {
    let dialect = SqliteDialect;
    assert_eq!(dialect.placeholder_prefix(), '?');
    assert!(dialect.numbered_placeholders());
}

#[test]
fn no_array_membership() {
    assert!(!SqliteDialect.supports_any_array());
}

#[test]
fn name_is_sqlite() {
    assert_eq!(SqliteDialect.name(), "sqlite");
}
