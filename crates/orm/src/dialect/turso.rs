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
    fn vector_bind(&self) -> (&'static str, &'static str) {
        ("vector32(", ")")
    }
    fn vector_distance(&self, metric: crate::vector::Distance) -> crate::vector::DistanceSql {
        use crate::vector::{Distance, DistanceSql::Function};
        match metric {
            Distance::L2 => Function("vector_distance_l2"),
            Distance::Cosine => Function("vector_distance_cos"),
            Distance::InnerProduct => Function("vector_distance_dot"),
        }
    }
}

#[cfg(test)]
#[path = "turso_test.rs"]
mod turso_test;
