//! Turso / libSQL dialect: SQLite-compatible (`?N` numbered, `IN (?, …)`), with
//! room for libSQL-specific extensions (e.g. vector search) layered on later.

use super::Dialect;

/// The Turso / `libSQL` dialect (SQLite-compatible wire syntax).
#[derive(Debug, Clone, Copy, Default)]
pub struct TursoDialect;

impl Dialect for TursoDialect {
    fn name(&self) -> &'static str {
        "turso"
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
#[path = "turso_test.rs"]
mod turso_test;
