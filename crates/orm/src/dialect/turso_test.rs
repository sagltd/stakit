use super::TursoDialect;
use crate::dialect::Dialect;

#[test]
fn sqlite_compatible_placeholders() {
    let dialect = TursoDialect;
    assert_eq!(dialect.placeholder_prefix(), '?');
    assert!(dialect.numbered_placeholders());
}

#[test]
fn name_is_turso() {
    assert_eq!(TursoDialect.name(), "turso");
}
