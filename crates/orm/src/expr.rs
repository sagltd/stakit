//! Typed predicates and operators: `eq`, `and`, `or`, comparisons, `any_of`,
//! `is_null`, plus `asc`/`desc` ordering.
//!
//! Values reach SQL only as binds (`$N`); identifiers only from [`Col`] tokens.
//! Predicates are an owned enum (no per-leaf closure box) rendered once at
//! terminal assembly.

use crate::ident::IdentError;
use crate::schema::Col;
use crate::sql::SqlWriter;
use crate::value::{ToValue, Value};

/// A column usable on the left of a comparison.
pub trait ColExpr {
    /// The column's Rust type, which the right-hand side must match.
    type Ty;
    /// Owning table name.
    fn table(&self) -> &'static str;
    /// Column name.
    fn name(&self) -> &'static str;
}

impl<T, Ty> ColExpr for Col<T, Ty> {
    type Ty = Ty;
    fn table(&self) -> &'static str {
        self.table
    }
    fn name(&self) -> &'static str {
        self.name
    }
}

/// A comparison operand: another column, or a bound value.
pub enum Operand {
    /// A table-qualified column reference.
    Column {
        /// Table name.
        table: &'static str,
        /// Column name.
        name: &'static str,
    },
    /// A backend-neutral value bound as a placeholder.
    Value(Value),
}

impl Operand {
    pub(crate) fn write_into(self, writer: &mut SqlWriter) -> Result<(), IdentError> {
        match self {
            Self::Column { table, name } => writer.push_qualified(table, name),
            Self::Value(value) => {
                writer.push_bind(value);
                Ok(())
            }
        }
    }
}

/// The right-hand side of a comparison against a column of type `Ty`.
///
/// Implemented for [`Col`] (column = column) and for a curated set of value
/// types (column = value). There is deliberately no reflexive blanket impl, so
/// type mismatches such as `eq(User::id /* Uuid */, 5_i32)` fail to compile.
pub trait IntoExpr<Ty> {
    /// Convert into a comparison operand.
    fn into_operand(self) -> Operand;
}

impl<T, Ty> IntoExpr<Ty> for Col<T, Ty> {
    fn into_operand(self) -> Operand {
        Operand::Column {
            table: self.table,
            name: self.name,
        }
    }
}

/// Implement `IntoExpr<$ty>` for an owned value type (binds itself).
macro_rules! value_into_expr {
    ($($ty:ty),* $(,)?) => {$(
        impl IntoExpr<$ty> for $ty {
            fn into_operand(self) -> Operand {
                Operand::Value(self.to_value())
            }
        }
        impl IntoExpr<Option<$ty>> for $ty {
            fn into_operand(self) -> Operand {
                Operand::Value(self.to_value())
            }
        }
    )*};
}

value_into_expr!(
    i16,
    i32,
    i64,
    f32,
    f64,
    bool,
    String,
    Vec<u8>,
    sqlx::types::Uuid,
    sqlx::types::chrono::DateTime<sqlx::types::chrono::Utc>,
    sqlx::types::chrono::NaiveDateTime,
    sqlx::types::chrono::NaiveDate,
    sqlx::types::chrono::NaiveTime,
);

/// Ergonomic string literals against `text` columns.
impl IntoExpr<String> for &str {
    fn into_operand(self) -> Operand {
        Operand::Value(self.to_value())
    }
}

impl IntoExpr<Option<String>> for &str {
    fn into_operand(self) -> Operand {
        Operand::Value(self.to_value())
    }
}

/// A boolean SQL predicate (owned tree, rendered once at terminal assembly).
pub enum Predicate {
    /// `"table"."name" <op> <rhs>`.
    Binary {
        /// Left column's table.
        table: &'static str,
        /// Left column's name.
        name: &'static str,
        /// Operator (e.g. ` = `), including surrounding spaces.
        op: &'static str,
        /// Right operand.
        rhs: Operand,
    },
    /// `"table"."name" IS NULL`.
    IsNull {
        /// Column table.
        table: &'static str,
        /// Column name.
        name: &'static str,
    },
    /// `"table"."name" = ANY($1)`.
    AnyOf {
        /// Column table.
        table: &'static str,
        /// Column name.
        name: &'static str,
        /// The bound array value.
        values: Value,
    },
    /// `(left AND right)`.
    And(Box<Self>, Box<Self>),
    /// `(left OR right)`.
    Or(Box<Self>, Box<Self>),
    /// `NOT (inner)`.
    Not(Box<Self>),
    /// Full-text match: FTS5 `column MATCH ?` (`SQLite`/Turso) or, on Postgres,
    /// `to_tsvector(column) @@ plainto_tsquery(?)` when [`stored`](Self::Match::stored)
    /// is `false`, or `column @@ plainto_tsquery(?)` against an already-stored
    /// `tsvector` column when it is `true`.
    Match {
        /// Column table.
        table: &'static str,
        /// Column name.
        name: &'static str,
        /// The search query text.
        query: String,
        /// Postgres text-search config (e.g. `"simple"`, `"spanish"`); `None`
        /// uses the dialect default. Ignored by FTS5.
        config: Option<&'static str>,
        /// Whether `column` is already a stored `tsvector` (Postgres): skip the
        /// query-time `to_tsvector(..)` and match the column directly with `@@`, so a
        /// GIN index on the stored column is usable. Ignored by FTS5 (no stored form).
        stored: bool,
    },
    /// `"table"."name" LIKE $N ESCAPE '\'` with `%`/`_`/`\` escaped in the bound
    /// value — a literal substring match (see [`contains`]).
    Like {
        /// Column table.
        table: &'static str,
        /// Column name.
        name: &'static str,
        /// The (escaped, `%…%`-wrapped) bound pattern.
        pattern: Value,
    },
    /// A `PostGIS` spatial predicate: `<func>("table"."name", $N::geometry[, $M])`,
    /// e.g. `ST_DWithin(...)` / `ST_Intersects(...)`. See [`crate::geo`].
    Spatial {
        /// Lowercase function name (e.g. `st_dwithin`).
        func: &'static str,
        /// Column table.
        table: &'static str,
        /// Column name.
        name: &'static str,
        /// The bound geometry value (rendered `$N::geometry`).
        geom: Value,
        /// An optional trailing bound distance argument (for `ST_DWithin`).
        distance: Option<f64>,
    },
    /// A raw SQL fragment (developer-trusted `&'static str`) — e.g. an aggregate
    /// `HAVING` like `count(*) > 5` that the typed builder can't model.
    Raw(&'static str),
}

/// A raw boolean SQL fragment (escape hatch; e.g. aggregate `HAVING`).
#[must_use]
pub const fn raw_pred(fragment: &'static str) -> Predicate {
    Predicate::Raw(fragment)
}

impl Predicate {
    /// `"table"."name" = <value>` from an already-converted [`Value`] (used by
    /// `Db::get` to filter on a table's primary-key column).
    #[must_use]
    pub const fn eq_value(table: &'static str, name: &'static str, value: Value) -> Self {
        Self::Binary {
            table,
            name,
            op: " = ",
            rhs: Operand::Value(value),
        }
    }

    /// Render this predicate into the writer.
    ///
    /// # Errors
    /// Returns [`IdentError`] if a column identifier is invalid.
    pub fn write(self, writer: &mut SqlWriter) -> Result<(), IdentError> {
        match self {
            Self::Binary {
                table,
                name,
                op,
                rhs,
            } => {
                writer.push_qualified(table, name)?;
                writer.push(op);
                rhs.write_into(writer)
            }
            Self::IsNull { table, name } => {
                writer.push_qualified(table, name)?;
                writer.push(" is null");
                Ok(())
            }
            Self::AnyOf {
                table,
                name,
                values,
            } => write_any_of(writer, table, name, values),
            Self::And(left, right) => combine(*left, " and ", *right, writer),
            Self::Or(left, right) => combine(*left, " or ", *right, writer),
            Self::Not(inner) => {
                writer.push("not (");
                inner.write(writer)?;
                writer.push(")");
                Ok(())
            }
            Self::Match {
                table,
                name,
                query,
                config,
                stored,
            } => write_match(writer, table, name, query, config, stored),
            Self::Like {
                table,
                name,
                pattern,
            } => write_like(writer, table, name, pattern),
            Self::Spatial {
                func,
                table,
                name,
                geom,
                distance,
            } => write_spatial(writer, func, table, name, geom, distance),
            Self::Raw(fragment) => {
                writer.push(fragment);
                Ok(())
            }
        }
    }
}

/// Render a full-text match per dialect: FTS5 `col MATCH ?`, or Postgres
/// `to_tsvector('<cfg>', col) @@ plainto_tsquery('<cfg>', $1)` — or, when `stored`,
/// `col @@ plainto_tsquery('<cfg>', $1)` against an already-stored `tsvector` column
/// (no query-time recompute, so a GIN index applies). `cfg` is a fixed `&'static`
/// config name (no injection); the query text is always bound.
///
/// An explicit `config` (e.g. `"spanish"`) overrides the dialect default for the
/// Postgres `TsQuery` path; it is ignored by FTS5.
fn write_match(
    writer: &mut SqlWriter,
    table: &'static str,
    name: &'static str,
    query: String,
    config: Option<&'static str>,
    stored: bool,
) -> Result<(), IdentError> {
    match writer.full_text() {
        crate::dialect::FullText::Fts5Match => {
            // FTS5 has no stored-tsvector form; both spellings match the column.
            writer.push_qualified(table, name)?;
            writer.push(" match ");
            writer.push_bind(Value::Text(query));
        }
        crate::dialect::FullText::TsQuery(default_config) => {
            let config = config.unwrap_or(default_config);
            if !stored {
                writer.push("to_tsvector('");
                writer.push(config);
                writer.push("', ");
            }
            writer.push_qualified(table, name)?;
            if !stored {
                writer.push(")");
            }
            writer.push(" @@ plainto_tsquery('");
            writer.push(config);
            writer.push("', ");
            writer.push_bind(Value::Text(query));
            writer.push(")");
        }
    }
    Ok(())
}

/// Render a Postgres `ts_rank(...)` full-text relevance score: `ts_rank(to_tsvector(
/// '<cfg>', "t"."col"), plainto_tsquery('<cfg>', $N))`, or — when `stored` — against
/// an already-stored `tsvector` column: `ts_rank("t"."col", plainto_tsquery('<cfg>',
/// $N))`. `cfg` is a fixed `&'static` config (no injection); the query text is bound.
/// Postgres-only: on an FTS5 dialect the bind is flagged so the terminal errors with
/// `Error::Unsupported` rather than emitting a non-existent function.
pub(crate) fn write_ts_rank(
    writer: &mut SqlWriter,
    table: &'static str,
    name: &'static str,
    query: &str,
    config: Option<&'static str>,
    stored: bool,
) -> Result<(), IdentError> {
    let crate::dialect::FullText::TsQuery(default_config) = writer.full_text() else {
        writer.mark_unsupported("ts_rank (Postgres full-text only)");
        // Still bind the value so placeholder numbering stays consistent; the terminal
        // returns `Error::Unsupported` before this SQL is dispatched.
        writer.push("ts_rank(");
        writer.push_qualified(table, name)?;
        writer.push(", ");
        writer.push_bind(Value::Text(query.to_owned()));
        writer.push(")");
        return Ok(());
    };
    let config = config.unwrap_or(default_config);
    writer.push("ts_rank(");
    if !stored {
        writer.push("to_tsvector('");
        writer.push(config);
        writer.push("', ");
    }
    writer.push_qualified(table, name)?;
    if !stored {
        writer.push(")");
    }
    writer.push(", plainto_tsquery('");
    writer.push(config);
    writer.push("', ");
    writer.push_bind(Value::Text(query.to_owned()));
    writer.push("))");
    Ok(())
}

/// Render a literal-substring `LIKE`: `"table"."name" like $N escape '\'`. The
/// pattern (already `%`/`_`/`\`-escaped and `%…%`-wrapped by [`contains`]) is
/// always parameter-bound; the `escape '\'` clause makes the escaping effective.
fn write_like(
    writer: &mut SqlWriter,
    table: &'static str,
    name: &'static str,
    pattern: Value,
) -> Result<(), IdentError> {
    writer.push_qualified(table, name)?;
    writer.push(" like ");
    writer.push_bind(pattern);
    writer.push(" escape '\\'");
    Ok(())
}

/// Render a `PostGIS` spatial predicate: `<func>("table"."name", $N::geometry)`,
/// plus a trailing bound distance (`, $M`) for `ST_DWithin`. The geometry and
/// distance are always parameter-bound — values never reach the SQL text.
fn write_spatial(
    writer: &mut SqlWriter,
    func: &'static str,
    table: &'static str,
    name: &'static str,
    geom: Value,
    distance: Option<f64>,
) -> Result<(), IdentError> {
    // `ST_*` only exists on Postgres/PostGIS; on other backends flag it so the
    // terminal returns `Error::Unsupported` rather than emitting SQL the DB rejects.
    if !writer.supports_spatial() {
        writer.mark_unsupported("PostGIS");
    }
    writer.push(func);
    writer.push("(");
    writer.push_qualified(table, name)?;
    writer.push(", ");
    writer.push_bind(geom);
    if let Some(distance) = distance {
        writer.push(", ");
        writer.push_bind(Value::F64(distance));
    }
    writer.push(")");
    Ok(())
}

/// Render list membership. On backends with array binds (Postgres) this is one
/// `= ANY($1)` array parameter; otherwise it expands to `IN (?, ?, …)` with one
/// bind per element. An empty list renders as the always-false `1 = 0`.
fn write_any_of(
    writer: &mut SqlWriter,
    table: &'static str,
    name: &'static str,
    values: crate::value::Value,
) -> Result<(), IdentError> {
    if writer.supports_any_array() {
        writer.push_qualified(table, name)?;
        writer.push(" = any(");
        writer.push_bind(values);
        writer.push(")");
        return Ok(());
    }
    let crate::value::Value::Array(_, items) = values else {
        // `any_of` always builds an Array; treat anything else as a single value.
        writer.push_qualified(table, name)?;
        writer.push(" in (");
        writer.push_bind(values);
        writer.push(")");
        return Ok(());
    };
    if items.is_empty() {
        writer.push("1 = 0");
        return Ok(());
    }
    writer.push_qualified(table, name)?;
    writer.push(" in (");
    for (index, item) in items.into_iter().enumerate() {
        if index > 0 {
            writer.push(", ");
        }
        writer.push_bind(item);
    }
    writer.push(")");
    Ok(())
}

fn combine(
    left: Predicate,
    op: &'static str,
    right: Predicate,
    writer: &mut SqlWriter,
) -> Result<(), IdentError> {
    writer.push("(");
    left.write(writer)?;
    writer.push(op);
    right.write(writer)?;
    writer.push(")");
    Ok(())
}

fn binary<L, R>(left: &L, op: &'static str, right: R) -> Predicate
where
    L: ColExpr + Copy,
    R: IntoExpr<L::Ty>,
{
    Predicate::Binary {
        table: left.table(),
        name: left.name(),
        op,
        rhs: right.into_operand(),
    }
}

/// `left = right`.
pub fn eq<L, R>(left: L, right: R) -> Predicate
where
    L: ColExpr + Copy,
    R: IntoExpr<L::Ty>,
{
    binary(&left, " = ", right)
}

/// `left <> right`.
pub fn ne<L, R>(left: L, right: R) -> Predicate
where
    L: ColExpr + Copy,
    R: IntoExpr<L::Ty>,
{
    binary(&left, " <> ", right)
}

/// `left > right`.
pub fn gt<L, R>(left: L, right: R) -> Predicate
where
    L: ColExpr + Copy,
    R: IntoExpr<L::Ty>,
{
    binary(&left, " > ", right)
}

/// `left < right`.
pub fn lt<L, R>(left: L, right: R) -> Predicate
where
    L: ColExpr + Copy,
    R: IntoExpr<L::Ty>,
{
    binary(&left, " < ", right)
}

/// `left >= right`.
pub fn gte<L, R>(left: L, right: R) -> Predicate
where
    L: ColExpr + Copy,
    R: IntoExpr<L::Ty>,
{
    binary(&left, " >= ", right)
}

/// `left <= right`.
pub fn lte<L, R>(left: L, right: R) -> Predicate
where
    L: ColExpr + Copy,
    R: IntoExpr<L::Ty>,
{
    binary(&left, " <= ", right)
}

/// `left LIKE pattern` for a `text` column (`String` or `Option<String>`).
pub fn like<L>(left: L, pattern: &str) -> Predicate
where
    L: ColExpr + Copy,
    String: IntoExpr<L::Ty>,
{
    binary(&left, " like ", pattern.to_owned())
}

/// `column = ANY($1)` — a single array bind for any list length (stable
/// statement, no per-element params; see the spec's statement-cache rules).
pub fn any_of<T, Ty>(column: Col<T, Ty>, values: &[Ty]) -> Predicate
where
    Ty: ToValue + crate::value::FromValue + Clone,
{
    Predicate::AnyOf {
        table: column.table,
        name: column.name,
        // Build the array Value directly from the slice — one allocation
        // instead of `to_vec()` (clone) + `to_value()` (second collect).
        values: crate::value::Value::Array(
            <Ty as crate::value::FromValue>::KIND,
            values.iter().cloned().map(ToValue::to_value).collect(),
        ),
    }
}

/// `column IS NULL`.
#[must_use]
pub const fn is_null<T, Ty>(column: Col<T, Ty>) -> Predicate {
    Predicate::IsNull {
        table: column.table,
        name: column.name,
    }
}

/// `(left AND right)`.
#[must_use]
pub fn and(left: Predicate, right: Predicate) -> Predicate {
    Predicate::And(Box::new(left), Box::new(right))
}

/// `(left OR right)`.
#[must_use]
pub fn or(left: Predicate, right: Predicate) -> Predicate {
    Predicate::Or(Box::new(left), Box::new(right))
}

/// `NOT (inner)` — negate any predicate (including compound `and`/`or` trees).
#[must_use]
pub fn not(inner: Predicate) -> Predicate {
    Predicate::Not(Box::new(inner))
}

/// Full-text search on `column` for `query`.
///
/// FTS5 `MATCH` on `SQLite`/Turso (the table must be an `fts5` virtual table), or
/// `to_tsvector @@ plainto_tsquery` on Postgres. Combine with `.order_by`/`.limit`.
#[must_use]
pub fn matches<T, Ty>(column: crate::schema::Col<T, Ty>, query: impl Into<String>) -> Predicate {
    Predicate::Match {
        table: column.table,
        name: column.name,
        query: query.into(),
        config: None,
        stored: false,
    }
}

/// Full-text search on `column` with an explicit Postgres text-search `config`
/// (e.g. `"simple"`, `"spanish"`, `"english"`).
///
/// `config` overrides the dialect default for the Postgres
/// `to_tsvector`/`plainto_tsquery` path; it is ignored by `SQLite`/Turso FTS5,
/// which has no per-query configuration. `config` is a developer-trusted
/// `&'static str` (never user input) and is never parameter-bound.
#[must_use]
pub fn matches_in<T, Ty>(
    column: crate::schema::Col<T, Ty>,
    query: impl Into<String>,
    config: &'static str,
) -> Predicate {
    Predicate::Match {
        table: column.table,
        name: column.name,
        query: query.into(),
        config: Some(config),
        stored: false,
    }
}

/// Full-text search against an **already-stored** `tsvector` column.
///
/// Point this at a `GENERATED ALWAYS AS (to_tsvector(..)) STORED` column (with a GIN
/// index): on Postgres it renders `"<t>"."<col>" @@ plainto_tsquery('<cfg>', $1)` — the
/// column is matched directly, with no query-time `to_tsvector` recompute, so the GIN
/// index is usable. On `SQLite`/Turso it falls back to FTS5 `MATCH` (there is no stored
/// `tsvector` form). The query text is always a bound parameter.
#[must_use]
pub fn matches_tsv<T, Ty>(
    column: crate::schema::Col<T, Ty>,
    query: impl Into<String>,
) -> Predicate {
    Predicate::Match {
        table: column.table,
        name: column.name,
        query: query.into(),
        config: None,
        stored: true,
    }
}

/// [`matches_tsv`] with an explicit Postgres text-search `config` (e.g. `"english"`).
///
/// `config` must match the configuration the stored `tsvector` was built with, so the
/// query lexemes align with the stored ones. It overrides the dialect default and is
/// ignored by `SQLite`/Turso FTS5. `config` is a developer-trusted `&'static str`
/// (never user input) and is never parameter-bound.
#[must_use]
pub fn matches_tsv_in<T, Ty>(
    column: crate::schema::Col<T, Ty>,
    query: impl Into<String>,
    config: &'static str,
) -> Predicate {
    Predicate::Match {
        table: column.table,
        name: column.name,
        query: query.into(),
        config: Some(config),
        stored: true,
    }
}

/// Case-sensitive literal-substring match: `column LIKE '%<needle>%'`.
///
/// The needle's `LIKE` metacharacters (`%`, `_`, `\`) are escaped so it matches
/// literally (no wildcard injection), then wrapped in `%…%` and bound as a single
/// parameter rendered with `escape '\'`. Use for "contains this text" filters; for
/// full-text ranking use [`matches`].
#[must_use]
pub fn contains<T, Ty>(column: crate::schema::Col<T, Ty>, needle: impl AsRef<str>) -> Predicate {
    let escaped = escape_like(needle.as_ref());
    Predicate::Like {
        table: column.table,
        name: column.name,
        pattern: Value::Text(format!("%{escaped}%")),
    }
}

/// Escape `LIKE` metacharacters (`\`, `%`, `_`) in `input` so it matches
/// literally under `escape '\'`. The backslash is escaped first so the escapes
/// introduced for `%`/`_` are not themselves double-escaped.
fn escape_like(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Sort direction.
#[derive(Debug, Clone, Copy)]
pub enum Direction {
    /// Ascending.
    Asc,
    /// Descending.
    Desc,
}

/// An `ORDER BY` term: a column and a direction.
#[derive(Debug, Clone, Copy)]
pub struct Order {
    table: &'static str,
    column: &'static str,
    direction: Direction,
}

impl Order {
    /// Render this term into the writer.
    ///
    /// # Errors
    /// Returns [`IdentError`] if a column identifier is invalid.
    pub fn write(&self, writer: &mut SqlWriter) -> Result<(), IdentError> {
        writer.push_qualified(self.table, self.column)?;
        writer.push(match self.direction {
            Direction::Asc => " asc",
            Direction::Desc => " desc",
        });
        Ok(())
    }
}

/// Ascending order on a column.
#[must_use]
pub const fn asc<T, Ty>(column: Col<T, Ty>) -> Order {
    Order {
        table: column.table,
        column: column.name,
        direction: Direction::Asc,
    }
}

/// Descending order on a column.
#[must_use]
pub const fn desc<T, Ty>(column: Col<T, Ty>) -> Order {
    Order {
        table: column.table,
        column: column.name,
        direction: Direction::Desc,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Predicate, and, any_of, asc, contains, desc, eq, gt, gte, is_null, like, lt, lte, matches,
        matches_in, matches_tsv, matches_tsv_in, ne, or, raw_pred,
    };
    use crate::dialect::{Dialect, SqliteDialect};
    use crate::schema::Col;
    use crate::sql::SqlWriter;
    use crate::value::Value;

    /// Render a predicate to its SQL string (binds discarded).
    fn render(predicate: Predicate) -> String {
        let mut writer = SqlWriter::new();
        predicate.write(&mut writer).unwrap();
        writer.sql().to_owned()
    }

    /// Render a predicate against an explicit dialect, returning SQL + binds.
    fn render_with(dialect: &'static dyn Dialect, predicate: Predicate) -> (String, Vec<Value>) {
        let mut writer = SqlWriter::with_dialect(dialect);
        predicate.write(&mut writer).unwrap();
        let (sql, binds) = writer.into_parts();
        (sql, binds.into_iter().collect())
    }

    const ID: Col<(), i32> = Col::new("t", "id");
    const NAME: Col<(), String> = Col::new("t", "name");
    const BIO: Col<(), Option<String>> = Col::new("t", "bio");

    #[test]
    fn comparison_operators_render_correct_symbols() {
        assert_eq!(render(eq(ID, 1_i32)), r#""t"."id" = $1"#);
        assert_eq!(render(ne(ID, 1_i32)), r#""t"."id" <> $1"#);
        assert_eq!(render(gt(ID, 1_i32)), r#""t"."id" > $1"#);
        assert_eq!(render(lt(ID, 1_i32)), r#""t"."id" < $1"#);
        assert_eq!(render(gte(ID, 1_i32)), r#""t"."id" >= $1"#);
        assert_eq!(render(lte(ID, 1_i32)), r#""t"."id" <= $1"#);
    }

    #[test]
    fn eq_column_to_column_emits_no_bind() {
        let mut writer = SqlWriter::new();
        let other: Col<(), i32> = Col::new("u", "id");
        eq(ID, other).write(&mut writer).unwrap();
        assert_eq!(writer.sql(), r#""t"."id" = "u"."id""#);
        assert_eq!(writer.bind_count(), 0);
    }

    #[test]
    fn like_on_nullable_renders_like() {
        assert_eq!(render(like(BIO, "%x%")), r#""t"."bio" like $1"#);
    }

    #[test]
    fn like_on_non_nullable_renders_like() {
        assert_eq!(render(like(NAME, "%x%")), r#""t"."name" like $1"#);
    }

    #[test]
    fn is_null_renders_is_null() {
        assert_eq!(render(is_null(BIO)), r#""t"."bio" is null"#);
    }

    #[test]
    fn any_of_multi_is_single_array_bind() {
        let mut writer = SqlWriter::new();
        any_of(ID, &[1_i32, 2, 3]).write(&mut writer).unwrap();
        assert_eq!(writer.sql(), r#""t"."id" = any($1)"#);
        assert_eq!(writer.bind_count(), 1);
    }

    #[test]
    fn any_of_empty_is_single_array_bind() {
        let mut writer = SqlWriter::new();
        let none: [i32; 0] = [];
        any_of(ID, &none).write(&mut writer).unwrap();
        assert_eq!(writer.sql(), r#""t"."id" = any($1)"#);
        assert_eq!(writer.bind_count(), 1);
    }

    #[test]
    fn and_or_nesting_parenthesizes() {
        let predicate = and(
            or(eq(ID, 1_i32), eq(ID, 2_i32)),
            and(eq(ID, 3_i32), eq(ID, 4_i32)),
        );
        assert_eq!(
            render(predicate),
            r#"(("t"."id" = $1 or "t"."id" = $2) and ("t"."id" = $3 and "t"."id" = $4))"#
        );
    }

    #[test]
    fn bind_numbering_follows_predicate_order() {
        let mut writer = SqlWriter::new();
        and(eq(ID, 10_i32), eq(NAME, "x"))
            .write(&mut writer)
            .unwrap();
        assert_eq!(writer.sql(), r#"("t"."id" = $1 and "t"."name" = $2)"#);
        assert_eq!(writer.bind_count(), 2);
    }

    #[test]
    fn raw_pred_renders_verbatim() {
        assert_eq!(render(raw_pred("count(*) > 5")), "count(*) > 5");
    }

    #[test]
    fn order_terms_render_direction() {
        let mut writer = SqlWriter::new();
        asc(ID).write(&mut writer).unwrap();
        assert_eq!(writer.sql(), r#""t"."id" asc"#);

        let mut writer = SqlWriter::new();
        desc(NAME).write(&mut writer).unwrap();
        assert_eq!(writer.sql(), r#""t"."name" desc"#);
    }

    #[test]
    fn contains_renders_like_with_escape_clause() {
        assert_eq!(
            render(contains(NAME, "abc")),
            r#""t"."name" like $1 escape '\'"#
        );
    }

    #[test]
    fn contains_escapes_like_metacharacters_in_bound_value() {
        // Backslash escaped first, then `%`/`_`, wrapped in `%…%`.
        let (sql, binds) =
            render_with(crate::dialect::default_dialect(), contains(NAME, r"50%_\x"));
        assert_eq!(sql, r#""t"."name" like $1 escape '\'"#);
        assert_eq!(binds.len(), 1);
        assert_eq!(binds[0], Value::Text(r"%50\%\_\\x%".to_owned()));
    }

    #[test]
    fn matches_default_config_on_postgres() {
        assert_eq!(
            render(matches(NAME, "hello")),
            r#"to_tsvector('english', "t"."name") @@ plainto_tsquery('english', $1)"#
        );
    }

    #[test]
    fn matches_in_overrides_config_on_postgres() {
        assert_eq!(
            render(matches_in(NAME, "hola", "spanish")),
            r#"to_tsvector('spanish', "t"."name") @@ plainto_tsquery('spanish', $1)"#
        );
    }

    #[test]
    fn matches_renders_fts5_on_sqlite() {
        let (sql, binds) = render_with(&SqliteDialect, matches(NAME, "hello"));
        assert_eq!(sql, r#""t"."name" match ?1"#);
        assert_eq!(binds, vec![Value::Text("hello".to_owned())]);
    }

    #[test]
    fn matches_in_config_ignored_by_fts5() {
        // FTS5 has no per-query config; `matches_in` renders identically to `matches`.
        let (sql, _) = render_with(&SqliteDialect, matches_in(NAME, "hello", "spanish"));
        assert_eq!(sql, r#""t"."name" match ?1"#);
    }

    #[test]
    fn matches_tsv_queries_stored_column_without_recompute_on_postgres() {
        // The stored tsvector is matched directly — no query-time `to_tsvector(..)`,
        // so a GIN index on the column is usable.
        assert_eq!(
            render(matches_tsv(NAME, "hello")),
            r#""t"."name" @@ plainto_tsquery('english', $1)"#
        );
    }

    #[test]
    fn matches_tsv_in_overrides_config_on_postgres() {
        assert_eq!(
            render(matches_tsv_in(NAME, "hola", "spanish")),
            r#""t"."name" @@ plainto_tsquery('spanish', $1)"#
        );
    }

    #[test]
    fn matches_tsv_falls_back_to_fts5_on_sqlite() {
        // SQLite has no stored-tsvector form; `matches_tsv` matches the column via FTS5.
        let (sql, binds) = render_with(&SqliteDialect, matches_tsv(NAME, "hello"));
        assert_eq!(sql, r#""t"."name" match ?1"#);
        assert_eq!(binds, vec![Value::Text("hello".to_owned())]);
    }
}
