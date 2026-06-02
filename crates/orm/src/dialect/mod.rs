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
}

/// The default dialect (Postgres), as a `'static` reference for the SQL writer.
#[must_use]
pub fn default_dialect() -> &'static dyn Dialect {
    &PostgresDialect
}
