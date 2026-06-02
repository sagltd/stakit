//! First-class vector search across backends (pgvector, Turso/libSQL, `sqlite-vec`).
//!
//! Store embeddings in a [`Vector`] column and run nearest-neighbour queries with
//! [`Select::nearest`](crate::Select::nearest) — the builder renders the right SQL
//! per backend: `$N::vector` + `<->`/`<=>`/`<#>` on pgvector, `vector32($N)` +
//! `vector_distance_*` on Turso, and `vec_distance_*` on `sqlite-vec`.
//!
//! ```no_run
//! use stakit_orm::prelude::*;
//! use stakit_orm::vector::{Vector, Distance};
//!
//! # async fn demo(db: Db) -> stakit_orm::Result<()> {
//! # #[derive(Table)] #[table(name="docs")]
//! # struct Doc { #[column(pk)] id: i64, #[column(sql_type="vector(3)")] embedding: Vector }
//! let q = [0.1_f32, 0.2, 0.3];
//! let nearest = db.find::<Doc>().nearest(Doc::embedding, &q, Distance::Cosine).limit(5).all().await?;
//! # let _ = nearest; Ok(()) }
//! ```

use crate::driver::Row;
use crate::error::{Error, Result};
use crate::projection::Projection;
use crate::schema::Col;
use crate::sql::SqlWriter;
use crate::value::{FromValue, ToValue, Value, ValueKind};

/// An embedding vector (`f32` components) usable as a column type and as the query
/// vector for nearest-neighbour search.
#[derive(Debug, Clone, PartialEq)]
pub struct Vector(pub Vec<f32>);

impl Vector {
    /// Wrap a list of components.
    #[must_use]
    pub fn new(components: impl Into<Vec<f32>>) -> Self {
        Self(components.into())
    }

    /// The underlying components.
    #[must_use]
    pub fn into_inner(self) -> Vec<f32> {
        self.0
    }
}

impl From<Vec<f32>> for Vector {
    fn from(value: Vec<f32>) -> Self {
        Self(value)
    }
}

impl<const N: usize> From<[f32; N]> for Vector {
    fn from(value: [f32; N]) -> Self {
        Self(value.to_vec())
    }
}

impl From<&[f32]> for Vector {
    fn from(value: &[f32]) -> Self {
        Self(value.to_vec())
    }
}

impl ToValue for Vector {
    fn to_value(self) -> Value {
        Value::Vector(self.0)
    }
}

impl FromValue for Vector {
    const KIND: ValueKind = ValueKind::Vector;
    fn from_value(value: Value) -> Result<Self> {
        match value {
            Value::Vector(components) => Ok(Self(components)),
            // Some backends return the vector as its text literal `[1,2,3]`.
            Value::Text(text) => parse_literal(&text).map(Self),
            other => Err(Error::Decode(
                format!("expected vector, got {other:?}").into(),
            )),
        }
    }
}

/// A vector distance metric for nearest-neighbour ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Distance {
    /// Euclidean (L2) distance.
    L2,
    /// Cosine distance.
    Cosine,
    /// (Negative) inner product.
    InnerProduct,
}

/// A **selectable** vector-distance projection (output `f64`).
///
/// Put it in a `select(...)` tuple or a `#[derive(Row)]` field to get the similarity
/// score back alongside the rows, not just order by it. Renders the same per-backend
/// SQL as [`Select::nearest`](crate::Select::nearest).
///
/// ```no_run
/// # use stakit_orm::prelude::*;
/// # use stakit_orm::vector::{distance, Distance};
/// # async fn d(db: Db) -> stakit_orm::Result<()> {
/// # #[derive(Table)] #[table(name="docs")]
/// # struct Doc { #[column(pk)] id: i64, #[column(sql_type="blob")] embedding: stakit_orm::Vector }
/// let q = [0.1_f32, 0.2, 0.3];
/// // returns Vec<(i64, f64)> — the id and its distance/score
/// let scored = db
///     .select((Doc::id, distance(Doc::embedding, &q, Distance::Cosine)))
///     .from::<Doc>()
///     .nearest(Doc::embedding, &q, Distance::Cosine)
///     .limit(5)
///     .all()
///     .await?;
/// # let _ = scored; Ok(()) }
/// ```
pub struct DistanceScore {
    table: &'static str,
    name: &'static str,
    query: Vec<f32>,
    metric: Distance,
}

/// Build a selectable [`DistanceScore`] for `column` against `query`.
#[must_use]
pub fn distance<T, Ty>(column: Col<T, Ty>, query: &[f32], metric: Distance) -> DistanceScore {
    DistanceScore {
        table: column.table,
        name: column.name,
        query: query.to_vec(),
        metric,
    }
}

impl Projection for DistanceScore {
    type Output = f64;
    fn arity(&self) -> usize {
        1
    }
    fn write_columns(&self, out: &mut SqlWriter) -> Result<()> {
        match out.vector_distance(self.metric) {
            DistanceSql::Operator(op) => {
                out.push_qualified(self.table, self.name)?;
                out.push(op);
                out.push_bind(Value::Vector(self.query.clone()));
            }
            DistanceSql::Function(function) => {
                out.push(function);
                out.push("(");
                out.push_qualified(self.table, self.name)?;
                out.push(", ");
                out.push_bind(Value::Vector(self.query.clone()));
                out.push(")");
            }
        }
        Ok(())
    }
    fn decode(&self, row: &dyn Row, start: usize) -> Result<f64> {
        crate::driver::decode_cell(row, start)
    }
}

/// How a backend spells a vector distance between a column and the query vector:
/// an infix operator (pgvector) or a function call (Turso / `sqlite-vec`).
#[derive(Debug, Clone, Copy)]
pub enum DistanceSql {
    /// `<col> <op> <query>` (e.g. pgvector `<->`).
    Operator(&'static str),
    /// `<fn>(<col>, <query>)` (e.g. Turso `vector_distance_cos`).
    Function(&'static str),
}

/// Render a `&[f32]` as the portable vector text literal `[1,2,3]` — accepted by
/// pgvector (`::vector`), Turso (`vector32`), and `sqlite-vec` (JSON).
#[must_use]
pub fn to_literal(components: &[f32]) -> String {
    use core::fmt::Write as _;
    let mut out = String::with_capacity(components.len() * 8 + 2);
    out.push('[');
    for (index, value) in components.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        // Write straight into `out` — no per-component throwaway String.
        // `{}` on f32 is round-trippable and avoids locale issues.
        let _ = write!(out, "{value}");
    }
    out.push(']');
    out
}

/// Parse a `[1,2,3]` vector text literal into components.
///
/// # Errors
/// Returns [`Error::Decode`] if the text is not a bracketed comma list of floats.
pub fn parse_literal(text: &str) -> Result<Vec<f32>> {
    let trimmed = text.trim();
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| Error::Decode(format!("invalid vector literal: {text:?}").into()))?;
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    inner
        .split(',')
        .map(|part| {
            part.trim()
                .parse::<f32>()
                .map_err(|error| Error::Decode(Box::new(error)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{parse_literal, to_literal};

    #[test]
    fn literal_round_trips() {
        let v = vec![1.0, -2.5, 0.333_333, 1e30, -0.0];
        assert_eq!(parse_literal(&to_literal(&v)).unwrap(), v);
    }

    #[test]
    fn empty_vector_round_trips() {
        assert_eq!(to_literal(&[]), "[]");
        assert_eq!(parse_literal("[]").unwrap(), Vec::<f32>::new());
        assert_eq!(parse_literal("[ ]").unwrap(), Vec::<f32>::new());
    }

    #[test]
    fn whitespace_is_tolerated() {
        assert_eq!(parse_literal("[1, 2 , 3]").unwrap(), vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn malformed_is_error() {
        assert!(parse_literal("1,2,3").is_err()); // no brackets
        assert!(parse_literal("[1,x,3]").is_err()); // non-numeric
    }
}
