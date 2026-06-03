//! SQL dialect differences across backends.
//!
//! The query builder is dialect-agnostic; only the small portable differences
//! (bind-placeholder syntax, list membership) live behind the [`Dialect`] trait.
//! Each backend has its own zero-sized impl in a sibling file.

mod mysql;
mod postgres;
mod sqlite;
mod turso;

pub use mysql::MySqlDialect;
pub use postgres::PostgresDialect;
pub use sqlite::SqliteDialect;
pub use turso::TursoDialect;

/// Per-backend SQL rendering differences. Object-safe; selected per connection.
pub trait Dialect: Send + Sync {
    /// Human-readable backend name (for diagnostics).
    fn name(&self) -> &'static str;

    /// The bind-placeholder lead character (`$` for Postgres, `?` otherwise).
    fn placeholder_prefix(&self) -> char;

    /// Whether placeholders carry their 1-based position (`$1`/`?1`) or are bare
    /// (`?`, MySQL-style).
    fn numbered_placeholders(&self) -> bool;

    /// Whether `= ANY($1)` with a single array bind is supported. When false,
    /// list membership expands to `IN (?, ?, …)` with one bind each.
    fn supports_any_array(&self) -> bool;

    /// The identifier quote character: `"` for Postgres / `SQLite` / Turso (SQL
    /// standard), `` ` `` for `MySQL`. Embedded occurrences are doubled.
    fn quote_char(&self) -> char {
        '"'
    }

    /// Whether `INSERT … RETURNING` is supported. Postgres, `SQLite` (>= 3.35), and
    /// Turso / `libSQL` support it; `MySQL` does not.
    fn supports_returning(&self) -> bool {
        true
    }

    /// Whether upserts use `MySQL`'s `ON DUPLICATE KEY UPDATE col = VALUES(col)`
    /// syntax instead of the standard `ON CONFLICT (target) DO …` (Postgres /
    /// `SQLite` / Turso). `MySQL` overrides this to `true`.
    fn upsert_on_duplicate_key(&self) -> bool {
        false
    }

    /// Text wrapped around a bound vector placeholder so the backend reads it as a
    /// vector: `("", "::vector")` on pgvector, `("vector32(", ")")` on Turso, `("",
    /// "")` (plain JSON text) on `sqlite-vec`. Default is no wrapping.
    fn vector_bind(&self) -> (&'static str, &'static str) {
        ("", "")
    }

    /// How this backend renders a vector distance between a column and the query
    /// vector. Default targets `sqlite-vec`'s `vec_distance_*` functions.
    fn vector_distance(&self, metric: crate::vector::Distance) -> crate::vector::DistanceSql {
        use crate::vector::{Distance, DistanceSql::Function};
        match metric {
            Distance::L2 | Distance::InnerProduct => Function("vec_distance_l2"),
            Distance::Cosine => Function("vec_distance_cosine"),
        }
    }

    /// Text wrapped around a bound `PostGIS` (E)WKT placeholder so the backend reads
    /// it as a geometry: `("", "::geometry")` on Postgres/`PostGIS`, no wrapping
    /// elsewhere (where the (E)WKT is just bound as plain text). See [`crate::geo`].
    fn geo_bind(&self) -> (&'static str, &'static str) {
        ("", "")
    }

    /// How this backend renders a full-text `matches(column, query)` predicate.
    /// Default is `SQLite`/Turso FTS5 (`column MATCH ?`).
    fn full_text(&self) -> FullText {
        FullText::Fts5Match
    }

    /// Whether this backend supports `PostGIS` spatial SQL — the `ST_*` predicate
    /// functions ([`crate::st_dwithin`] etc.) and the `<->` KNN operator
    /// ([`crate::Select::nearest_geo`]). Only Postgres overrides this to `true`;
    /// everywhere else those builders error early with `Error::Unsupported`.
    /// (The [`crate::GeoPoint`] column type itself works on every backend — it
    /// stores as plain WKT text — this only gates the spatial *operators*.)
    fn supports_spatial(&self) -> bool {
        false
    }
}

/// How a backend renders a full-text `matches(column, query)` predicate.
#[derive(Debug, Clone, Copy)]
pub enum FullText {
    /// `SQLite`/Turso FTS5: `column MATCH ?`.
    Fts5Match,
    /// Postgres: `to_tsvector('<cfg>', column) @@ plainto_tsquery('<cfg>', $1)`.
    TsQuery(&'static str),
}

/// The default dialect (Postgres), as a `'static` reference for the SQL writer.
#[must_use]
pub fn default_dialect() -> &'static dyn Dialect {
    &PostgresDialect
}
