//! Select projections: what `select(...)` returns is decided by the argument's
//! [`Projection::Output`], so no type annotation is needed at the call site.
//!
//! - [`Col`] / [`Count`] / [`Agg`] are single-column projections.
//! - Tuples combine any projections, decoded **positionally**, so partial selects
//!   (`(Uuid, String)`) and whole-row joins (`(User, Post)`) both work.
//! - [`All`] is a whole-row projection (`T` via positional decode);
//!   `.nullable()` yields `Option<T>` for outer-join sides.

use crate::driver::Row;
use crate::error::Result;
use crate::schema::{Col, Table};
use crate::sql::SqlWriter;
use crate::value::FromValue;
use core::marker::PhantomData;

use crate::driver::decode_cell as decode_at;

/// A select projection: its column list and how to decode one row.
pub trait Projection {
    /// The Rust type one row decodes to.
    type Output;
    /// Number of select-list columns this projection occupies.
    fn arity(&self) -> usize;
    /// Append this projection's select-list fragments.
    ///
    /// # Errors
    /// Returns an error if a column identifier is invalid.
    fn write_columns(&self, out: &mut SqlWriter) -> Result<()>;
    /// Decode this projection's value, reading from column ordinal `start`.
    ///
    /// # Errors
    /// Returns an error if a column fails to decode.
    fn decode(&self, row: &dyn Row, start: usize) -> Result<Self::Output>;
}

impl<T, Ty> Projection for Col<T, Ty>
where
    Ty: FromValue,
{
    type Output = Ty;
    fn arity(&self) -> usize {
        1
    }
    fn write_columns(&self, out: &mut SqlWriter) -> Result<()> {
        out.push_qualified(self.table, self.name)?;
        // A composite/custom type reads via a cast (e.g. `col::text`) so the cell
        // arrives in the form its `FromValue` decodes. Only on cast-capable dialects.
        if let Some(cast) = <Ty as FromValue>::READ_CAST {
            if out.supports_cast() {
                out.push("::");
                out.push(cast);
            }
        }
        Ok(())
    }
    fn decode(&self, row: &dyn Row, start: usize) -> Result<Ty> {
        decode_at(row, start)
    }
}

/// `count(*)` projection (`i64`).
pub struct Count;

/// `count(*)`.
#[must_use]
pub const fn count() -> Count {
    Count
}

impl Projection for Count {
    type Output = i64;
    fn arity(&self) -> usize {
        1
    }
    fn write_columns(&self, out: &mut SqlWriter) -> Result<()> {
        out.push("count(*)");
        Ok(())
    }
    fn decode(&self, row: &dyn Row, start: usize) -> Result<i64> {
        decode_at(row, start)
    }
}

/// An aggregate over one column (`min`/`max`/`count(col)`), decoding to `Out`.
pub struct Agg<Out> {
    func: &'static str,
    table: &'static str,
    name: &'static str,
    marker: PhantomData<fn() -> Out>,
}

impl<Out> Agg<Out> {
    const fn new(func: &'static str, table: &'static str, name: &'static str) -> Self {
        Self {
            func,
            table,
            name,
            marker: PhantomData,
        }
    }
}

impl<Out> Projection for Agg<Out>
where
    Out: FromValue,
{
    type Output = Out;
    fn arity(&self) -> usize {
        1
    }
    fn write_columns(&self, out: &mut SqlWriter) -> Result<()> {
        out.push(self.func);
        out.push("(");
        out.push_qualified(self.table, self.name)?;
        out.push(")");
        Ok(())
    }
    fn decode(&self, row: &dyn Row, start: usize) -> Result<Out> {
        decode_at(row, start)
    }
}

/// `min(column)` — `NULL` (`None`) over zero rows, so the output is `Option<Ty>`.
#[must_use]
pub const fn min<T, Ty>(column: Col<T, Ty>) -> Agg<Option<Ty>> {
    Agg::new("min", column.table, column.name)
}

/// `max(column)` — `Option<Ty>` (nullable over empty input).
#[must_use]
pub const fn max<T, Ty>(column: Col<T, Ty>) -> Agg<Option<Ty>> {
    Agg::new("max", column.table, column.name)
}

/// `count(column)` (non-null rows only) → `i64`.
#[must_use]
pub const fn count_col<T, Ty>(column: Col<T, Ty>) -> Agg<i64> {
    Agg::new("count", column.table, column.name)
}

/// `sum(column)`. Postgres widens the result (e.g. `sum(int)` → `bigint`), so the
/// decoded type `Out` is caller-chosen (usually inferred from the result binding)
/// and nullable over empty input.
#[must_use]
pub const fn sum<Out, T, Ty>(column: Col<T, Ty>) -> Agg<Out> {
    Agg::new("sum", column.table, column.name)
}

/// `avg(column)`. The decoded type `Out` is caller-chosen (Postgres returns
/// `numeric`/`double precision`); nullable over empty input.
#[must_use]
pub const fn avg<Out, T, Ty>(column: Col<T, Ty>) -> Agg<Out> {
    Agg::new("avg", column.table, column.name)
}

/// A raw SQL expression projection (the `sql!` capability) decoding to `Out`.
///
/// The `fragment` is written verbatim into the select list — the developer's
/// responsibility (a `&'static str`, like the raw escape hatch). Use for SQL the
/// builder doesn't model, e.g. `extract(year from "posts"."created_at")`.
pub struct SqlExpr<Out> {
    fragment: &'static str,
    marker: PhantomData<fn() -> Out>,
}

/// Build a raw SQL expression projection (see [`SqlExpr`]).
#[must_use]
pub const fn sql_expr<Out>(fragment: &'static str) -> SqlExpr<Out> {
    SqlExpr {
        fragment,
        marker: PhantomData,
    }
}

impl<Out> Projection for SqlExpr<Out>
where
    Out: FromValue,
{
    type Output = Out;
    fn arity(&self) -> usize {
        1
    }
    fn write_columns(&self, out: &mut SqlWriter) -> Result<()> {
        out.push(self.fragment);
        Ok(())
    }
    fn decode(&self, row: &dyn Row, start: usize) -> Result<Out> {
        decode_at(row, start)
    }
}

/// A Postgres `ts_rank(...)` full-text relevance score, decoded as `f32`.
///
/// Selectable like any projection. Build with [`ts_rank`] / [`ts_rank_stored`] /
/// [`ts_rank_in`]; order by it with [`Select::order_by_rank`](crate::Select::order_by_rank).
/// Postgres-only — selecting it on another backend errors with `Error::Unsupported`.
pub struct TsRank {
    table: &'static str,
    name: &'static str,
    query: String,
    config: Option<&'static str>,
    stored: bool,
}

/// `ts_rank` of `column` against `query`, computing `to_tsvector` at query time (use
/// for a plain `text` column). For a stored `tsvector` column use [`ts_rank_stored`].
#[must_use]
pub fn ts_rank<T, Ty>(column: Col<T, Ty>, query: impl Into<String>) -> TsRank {
    TsRank {
        table: column.table,
        name: column.name,
        query: query.into(),
        config: None,
        stored: false,
    }
}

/// `ts_rank` against an already-stored `tsvector` column (no query-time recompute, so
/// a GIN index applies) — e.g. a generated `desc_tsv` column.
#[must_use]
pub fn ts_rank_stored<T, Ty>(column: Col<T, Ty>, query: impl Into<String>) -> TsRank {
    TsRank {
        table: column.table,
        name: column.name,
        query: query.into(),
        config: None,
        stored: true,
    }
}

/// [`ts_rank`] with an explicit Postgres text-search `config` (e.g. `"spanish"`).
#[must_use]
pub fn ts_rank_in<T, Ty>(
    column: Col<T, Ty>,
    query: impl Into<String>,
    config: &'static str,
) -> TsRank {
    TsRank {
        table: column.table,
        name: column.name,
        query: query.into(),
        config: Some(config),
        stored: false,
    }
}

impl TsRank {
    /// Render this rank expression into `out` (shared by the projection and
    /// [`Select::order_by_rank`](crate::Select::order_by_rank)).
    pub(crate) fn write(&self, out: &mut SqlWriter) -> Result<()> {
        crate::expr::write_ts_rank(
            out,
            self.table,
            self.name,
            &self.query,
            self.config,
            self.stored,
        )?;
        Ok(())
    }
}

impl Projection for TsRank {
    type Output = f32;
    fn arity(&self) -> usize {
        1
    }
    fn write_columns(&self, out: &mut SqlWriter) -> Result<()> {
        self.write(out)
    }
    fn decode(&self, row: &dyn Row, start: usize) -> Result<f32> {
        decode_at(row, start)
    }
}

/// Marker: a non-nullable whole-row projection.
pub struct NotNull;
/// Marker: a nullable whole-row projection (outer-join side).
pub struct Nullable;

/// Whole-row projection for table `T` (selects every column).
pub struct All<T, N = NotNull> {
    marker: PhantomData<fn() -> (T, N)>,
}

impl<T> All<T, NotNull> {
    /// Construct a whole-row projection (generated as `T::all()`).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            marker: PhantomData,
        }
    }

    /// Treat this as the nullable side of an outer join (`Option<T>`).
    #[must_use]
    pub const fn nullable(self) -> All<T, Nullable> {
        All {
            marker: PhantomData,
        }
    }
}

impl<T> Default for All<T, NotNull> {
    fn default() -> Self {
        Self::new()
    }
}

fn write_all_columns<T: Table>(out: &mut SqlWriter) -> Result<()> {
    for (index, column) in T::COLUMNS.iter().enumerate() {
        if index > 0 {
            out.push(", ");
        }
        out.push_qualified(T::TABLE, column.name)?;
        // Composite/custom columns read via a cast (`col::text`) so the cell decodes
        // through their `FromValue`. Only on cast-capable dialects (Postgres).
        if let Some(cast) = column.read_cast {
            if out.supports_cast() {
                out.push("::");
                out.push(cast);
            }
        }
    }
    Ok(())
}

/// Whether the outer-join side is "absent" (no matching row): the non-nullable PK
/// column is NULL, or — for a PK-less table — every projected column is NULL.
fn outer_join_absent<T: Table>(row: &dyn Row, start: usize) -> Result<bool> {
    if let Some(pk) = T::COLUMNS.iter().position(|column| column.is_pk) {
        return row.is_null(start + pk);
    }
    for index in 0..T::COLUMNS.len() {
        if !row.is_null(start + index)? {
            return Ok(false);
        }
    }
    Ok(true)
}

impl<T: Table> Projection for All<T, NotNull> {
    type Output = T;
    fn arity(&self) -> usize {
        T::COLUMNS.len()
    }
    fn write_columns(&self, out: &mut SqlWriter) -> Result<()> {
        write_all_columns::<T>(out)
    }
    fn decode(&self, row: &dyn Row, start: usize) -> Result<T> {
        // Positional decode so this works at any offset inside a join tuple.
        T::from_row_at(row, start)
    }
}

impl<T: Table> Projection for All<T, Nullable> {
    type Output = Option<T>;
    fn arity(&self) -> usize {
        T::COLUMNS.len()
    }
    fn write_columns(&self, out: &mut SqlWriter) -> Result<()> {
        write_all_columns::<T>(out)
    }
    fn decode(&self, row: &dyn Row, start: usize) -> Result<Option<T>> {
        if outer_join_absent::<T>(row, start)? {
            return Ok(None);
        }
        Ok(Some(T::from_row_at(row, start)?))
    }
}

/// Implement `Projection` for a tuple of projections, decoding each at its
/// positional offset (so whole-row `All<T>` members work alongside leaf columns).
macro_rules! tuple_projection {
    ($($name:ident => $index:tt),+) => {
        impl<$($name),+> Projection for ($($name,)+)
        where
            $($name: Projection,)+
        {
            type Output = ($($name::Output,)+);
            fn arity(&self) -> usize {
                0 $(+ self.$index.arity())+
            }
            fn write_columns(&self, out: &mut SqlWriter) -> Result<()> {
                let mut first = true;
                $(
                    if !first {
                        out.push(", ");
                    }
                    first = false;
                    self.$index.write_columns(out)?;
                )+
                let _ = first;
                Ok(())
            }
            #[allow(unused_assignments)]
            fn decode(&self, row: &dyn Row, start: usize) -> Result<Self::Output> {
                let mut offset = start;
                Ok(($(
                    {
                        let value = self.$index.decode(row, offset)?;
                        offset += self.$index.arity();
                        value
                    },
                )+))
            }
        }
    };
}

tuple_projection!(A => 0);
tuple_projection!(A => 0, B => 1);
tuple_projection!(A => 0, B => 1, C => 2);
tuple_projection!(A => 0, B => 1, C => 2, D => 3);
tuple_projection!(A => 0, B => 1, C => 2, D => 3, E => 4);
tuple_projection!(A => 0, B => 1, C => 2, D => 3, E => 4, F => 5);
