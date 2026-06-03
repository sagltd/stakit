//! `MySQL` dialect: bare `?` positional placeholders, `IN (?, …)` membership.

use super::Dialect;

/// The `MySQL` dialect.
#[derive(Debug, Clone, Copy, Default)]
pub struct MySqlDialect;

impl Dialect for MySqlDialect {
    fn name(&self) -> &'static str {
        "mysql"
    }
    fn placeholder_prefix(&self) -> char {
        '?'
    }
    fn numbered_placeholders(&self) -> bool {
        false
    }
    fn supports_any_array(&self) -> bool {
        false
    }
    fn quote_char(&self) -> char {
        '`'
    }
    fn supports_returning(&self) -> bool {
        false
    }
    fn upsert_on_duplicate_key(&self) -> bool {
        true
    }
}

#[cfg(test)]
#[path = "mysql_test.rs"]
mod mysql_test;
