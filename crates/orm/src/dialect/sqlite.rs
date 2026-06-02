//! `SQLite` dialect: `?N` numbered placeholders, `IN (?, …)` membership.

use super::Dialect;

/// The `SQLite` dialect.
#[derive(Debug, Clone, Copy, Default)]
pub struct SqliteDialect;

impl Dialect for SqliteDialect {
    fn name(&self) -> &'static str {
        "sqlite"
    }
    fn placeholder_prefix(&self) -> char {
        '?'
    }
    fn numbered_placeholders(&self) -> bool {
        true
    }
    fn supports_any_array(&self) -> bool {
        false
    }
}

#[cfg(test)]
#[path = "sqlite_test.rs"]
mod sqlite_test;
