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
    fn vector_bind(&self) -> (&'static str, &'static str) {
        ("", "::vector")
    }
    fn geo_bind(&self) -> (&'static str, &'static str) {
        ("", "::geometry")
    }
    fn vector_distance(&self, metric: crate::vector::Distance) -> crate::vector::DistanceSql {
        use crate::vector::{Distance, DistanceSql::Operator};
        match metric {
            Distance::L2 => Operator(" <-> "),
            Distance::Cosine => Operator(" <=> "),
            Distance::InnerProduct => Operator(" <#> "),
        }
    }
    fn full_text(&self) -> super::FullText {
        super::FullText::TsQuery("english")
    }
    fn supports_spatial(&self) -> bool {
        true
    }
}

#[cfg(test)]
#[path = "postgres_test.rs"]
mod postgres_test;
