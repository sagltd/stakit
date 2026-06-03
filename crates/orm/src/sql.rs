//! SQL assembly: a single growing string plus an ordered, typed bind buffer.
//!
//! The whole statement is assembled once (at a terminal), never per builder
//! step, and `$N` placeholders are numbered globally in clause order.

use crate::ident::{self, IdentError};
use crate::value::Value;

/// Inline-capacity bind buffer: most statements bind few values, so the common
/// case stays on the stack (no heap allocation for the buffer itself). Values are
/// backend-neutral [`Value`]s; each driver converts them to native parameters.
pub(crate) type BindBuffer = smallvec::SmallVec<[Value; 4]>;

/// Typical assembled statement length; pre-sizing avoids `String` reallocs in
/// the build hot path.
const DEFAULT_SQL_CAPACITY: usize = 96;

/// Accumulates SQL text and its ordered bind values.
///
/// The dialect's per-statement flags are read **once** at construction and cached
/// as plain fields, so the hot assembly path (`push_bind`, `push_ident`) does no
/// vtable dispatch per bind/identifier.
#[allow(clippy::struct_excessive_bools)] // cached per-dialect rendering flags
pub struct SqlWriter {
    sql: String,
    binds: BindBuffer,
    placeholder_prefix: char,
    numbered_placeholders: bool,
    quote_char: char,
    supports_any_array: bool,
    supports_spatial: bool,
    supports_cast: bool,
    vector_bind: (&'static str, &'static str),
    geo_bind: (&'static str, &'static str),
    // Set when a clause needs a backend feature this dialect lacks (e.g. PostGIS
    // `ST_*` / `<->` on a non-Postgres backend); the terminal turns it into an
    // early `Error::Unsupported` instead of emitting SQL that only fails at the DB.
    unsupported: Option<&'static str>,
    // Kept for the rare vector-distance path (metric-dependent, can't be a flag).
    dialect: &'static dyn crate::dialect::Dialect,
}

impl Default for SqlWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl SqlWriter {
    /// Create a writer with a pre-sized SQL buffer (Postgres dialect).
    #[must_use]
    pub fn new() -> Self {
        Self::with_dialect(crate::dialect::default_dialect())
    }

    /// Create a writer rendering placeholders for `dialect`. The dialect's flags
    /// are snapshotted here; later calls never re-dispatch through the trait object.
    #[must_use]
    pub fn with_dialect(dialect: &'static dyn crate::dialect::Dialect) -> Self {
        Self {
            sql: String::with_capacity(DEFAULT_SQL_CAPACITY),
            binds: BindBuffer::new(),
            placeholder_prefix: dialect.placeholder_prefix(),
            numbered_placeholders: dialect.numbered_placeholders(),
            quote_char: dialect.quote_char(),
            supports_any_array: dialect.supports_any_array(),
            supports_spatial: dialect.supports_spatial(),
            supports_cast: dialect.supports_cast(),
            vector_bind: dialect.vector_bind(),
            geo_bind: dialect.geo_bind(),
            unsupported: None,
            dialect,
        }
    }

    /// Whether this backend supports `PostGIS` spatial SQL (`ST_*` functions and
    /// the `<->` KNN operator). Only Postgres (with the extension) does.
    #[must_use]
    pub const fn supports_spatial(&self) -> bool {
        self.supports_spatial
    }

    /// Record that a clause requires a backend feature this dialect lacks. The
    /// first such marker wins; the terminal surfaces it as [`crate::Error::Unsupported`].
    pub const fn mark_unsupported(&mut self, feature: &'static str) {
        if self.unsupported.is_none() {
            self.unsupported = Some(feature);
        }
    }

    /// The unsupported-feature marker, if any clause set one.
    #[must_use]
    pub const fn unsupported(&self) -> Option<&'static str> {
        self.unsupported
    }

    /// How this backend renders a vector distance (for nearest-neighbour ordering).
    #[must_use]
    pub fn vector_distance(&self, metric: crate::vector::Distance) -> crate::vector::DistanceSql {
        self.dialect.vector_distance(metric)
    }

    /// How this backend renders a full-text match predicate.
    #[must_use]
    pub fn full_text(&self) -> crate::dialect::FullText {
        self.dialect.full_text()
    }

    /// Whether this backend uses `MySQL`'s `ON DUPLICATE KEY UPDATE` upsert syntax.
    #[must_use]
    pub fn upsert_on_duplicate_key(&self) -> bool {
        self.dialect.upsert_on_duplicate_key()
    }

    /// Append raw SQL text (keywords, punctuation — never user values).
    pub fn push(&mut self, text: &str) {
        self.sql.push_str(text);
    }

    /// Append a single quoted identifier.
    ///
    /// # Errors
    /// Returns [`IdentError`] if the identifier is invalid.
    pub fn push_ident(&mut self, name: &str) -> Result<(), IdentError> {
        ident::write_quoted_with(&mut self.sql, name, self.quote_char)
    }

    /// Append a table-qualified column: `"table"."column"`.
    ///
    /// # Errors
    /// Returns [`IdentError`] if either identifier is invalid.
    pub fn push_qualified(&mut self, table: &str, column: &str) -> Result<(), IdentError> {
        let quote = self.quote_char;
        ident::write_quoted_with(&mut self.sql, table, quote)?;
        self.sql.push('.');
        ident::write_quoted_with(&mut self.sql, column, quote)
    }

    /// Queue a bind value and write its positional placeholder (`$N` for
    /// Postgres, `?N` for SQLite/libSQL).
    pub fn push_bind(&mut self, value: Value) {
        // A cast-tagged value renders `$N::<cast>` (Postgres composite/enum/domain/
        // custom types). The driver only ever sees the unwrapped `inner`, so no
        // driver needs a `Value::Cast` arm. On a dialect without `::` casts the bind
        // is flagged so the terminal returns `Error::Unsupported`.
        if let Value::Cast { cast, inner } = value {
            if self.supports_cast {
                self.binds.push(*inner);
                let position = self.binds.len();
                self.push_placeholder(position);
                self.sql.push_str("::");
                self.sql.push_str(cast);
            } else {
                self.mark_unsupported(cast);
                self.binds.push(*inner);
                let position = self.binds.len();
                self.push_placeholder(position);
            }
            return;
        }
        // `PostGIS` geometries get the most involved treatment: a `::geometry` cast,
        // wrapped in `ST_SetSRID(.., srid)` when a SRID is attached. Other backends
        // return `("", "")` from `geo_bind`, so the WKT just binds as plain text.
        if let Value::Geo { srid, .. } = &value {
            let srid = *srid;
            self.push_geo_bind(value, srid);
            return;
        }
        // Vectors are wrapped so the backend reads the placeholder as a vector
        // (`$N::vector` / `vector32($N)` / plain), both on insert and in queries.
        let wrap = if matches!(value, Value::Vector(_)) {
            self.vector_bind
        } else {
            ("", "")
        };
        self.binds.push(value);
        let position = self.binds.len();
        self.sql.push_str(wrap.0);
        self.push_placeholder(position);
        self.sql.push_str(wrap.1);
    }

    /// Render a `PostGIS` geometry bind: `<pre>$N<suf>`, optionally wrapped in
    /// `ST_SetSRID(.., srid)` so a tagged SRID survives. The `(pre, suf)` cast comes
    /// from the dialect (`("", "::geometry")` on Postgres; `("", "")` elsewhere).
    fn push_geo_bind(&mut self, value: Value, srid: Option<i32>) {
        let (pre, suf) = self.geo_bind;
        // Only wrap in ST_SetSRID when the backend actually casts to geometry; on
        // plain-text backends the value is just the WKT string.
        let set_srid = srid.filter(|_| !pre.is_empty() || !suf.is_empty());
        self.binds.push(value);
        let position = self.binds.len();
        if set_srid.is_some() {
            self.sql.push_str("ST_SetSRID(");
        }
        self.sql.push_str(pre);
        self.push_placeholder(position);
        self.sql.push_str(suf);
        if let Some(srid) = set_srid {
            use core::fmt::Write as _;
            let _ = write!(self.sql, ", {srid})");
        }
    }

    /// Write the placeholder for a bind at 1-based `position` (`$N` / `?N` / `?`).
    fn push_placeholder(&mut self, position: usize) {
        self.sql.push(self.placeholder_prefix);
        if self.numbered_placeholders {
            // Avoid `format!` allocation in the hot path.
            let mut buffer = itoa_buffer();
            self.sql.push_str(itoa(position, &mut buffer));
        }
    }

    /// Number of queued binds (also the next `$N`).
    #[must_use]
    pub fn bind_count(&self) -> usize {
        self.binds.len()
    }

    /// Whether this backend supports `= ANY(<array>)` with one array bind. When
    /// false, list membership must expand to `IN (?, ?, …)`.
    #[must_use]
    pub const fn supports_any_array(&self) -> bool {
        self.supports_any_array
    }

    /// Whether this backend supports `$N::type` / `expr::type` casts (Postgres).
    /// Used to gate read-side projection casts (e.g. composite `col::text`).
    #[must_use]
    pub const fn supports_cast(&self) -> bool {
        self.supports_cast
    }

    /// Borrow the assembled SQL text.
    #[must_use]
    pub fn sql(&self) -> &str {
        &self.sql
    }

    /// Consume into assembled SQL and its bind buffer.
    #[must_use]
    pub fn into_parts(self) -> (String, BindBuffer) {
        (self.sql, self.binds)
    }
}

/// A stack buffer large enough for any `usize` rendered as decimal.
type ItoaBuffer = [u8; 20];

const fn itoa_buffer() -> ItoaBuffer {
    [0; 20]
}

/// Render `value` into `buffer` and return the decimal substring.
fn itoa(value: usize, buffer: &mut ItoaBuffer) -> &str {
    if value == 0 {
        buffer[0] = b'0';
        return core::str::from_utf8(&buffer[..1]).unwrap_or("0");
    }
    let mut index = buffer.len();
    let mut remaining = value;
    while remaining > 0 {
        index -= 1;
        buffer[index] = b'0' + u8::try_from(remaining % 10).unwrap_or(0);
        remaining /= 10;
    }
    core::str::from_utf8(&buffer[index..]).unwrap_or("0")
}

#[cfg(test)]
#[path = "sql/sql_test.rs"]
mod sql_test;
