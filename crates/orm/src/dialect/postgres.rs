//! Postgres dialect: `$N` numbered placeholders, native `= ANY($1)`.

use super::Dialect;

/// The `PostgreSQL` dialect.
#[derive(Debug, Clone, Copy, Default)]
pub struct PostgresDialect;

impl Dialect for PostgresDialect {
    fn name(&self) -> &'static str {
        "postgres"
    }
    fn placeholder_prefix(&self) -> char {
        '$'
    }
    fn numbered_placeholders(&self) -> bool {
        true
    }
    fn supports_any_array(&self) -> bool {
        true
    }
}

#[cfg(test)]
#[path = "postgres_test.rs"]
mod postgres_test;
