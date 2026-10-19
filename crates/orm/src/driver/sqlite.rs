//! The `SQLite` backend (sqlx): [`Row`] for `SqliteRow`, the [`SqliteDriver`], its
//! transaction handle, and `Value` → `SqliteArguments` binding.
//!
//! `SQLite` stores a narrow set of native types, so a few [`Value`] kinds map onto
//! wider storage: `f32` round-trips through `REAL` (`f64`), and arrays are
//! unsupported (list membership is expanded to `IN (?, …)` by the dialect, so the
//! array bind path is never hit for `SQLite`).

use super::sqlx_backend::{
    add_null_scalar, add_scalar, array_from_json_text, geo_from_text, into_decode, read_scalar,
    sqlx_driver, text_cell, vector_from_text,
};
use crate::dialect::SqliteDialect;
use crate::error::{Error, Result};
use crate::value::{Value, ValueKind};
use sqlx::sqlite::{SqliteArguments, SqlitePool, SqliteRow};
use sqlx::{Arguments, Row as _, Sqlite};

fn try_value(row: &SqliteRow, index: usize, kind: ValueKind) -> Result<Value> {
    match kind {
        // SQLite has no 32-bit float; widen through REAL.
        ValueKind::F32 => {
            let cell: Option<f64> = row.try_get(index).map_err(into_decode)?;
            #[allow(clippy::cast_possible_truncation)]
            Ok(cell.map_or(Value::Null(kind), |value| Value::F32(value as f32)))
        }
        ValueKind::Vector => vector_from_text(text_cell(row, index)?),
        // SQLite has no `PostGIS`; the WKT round-trips as plain text.
        ValueKind::Geo => Ok(geo_from_text(text_cell(row, index)?)),
        // No native arrays on SQLite — stored as JSON text.
        ValueKind::Array(elem) => array_from_json_text(text_cell(row, index)?, kind, *elem),
        _ => read_scalar!(row, index, kind),
    }
}

sqlx_driver! {
    row: SqliteRow;
    /// The `SQLite` [`Driver`](super::Driver), wrapping an sqlx pool.
    pub struct SqliteDriver(pool: SqlitePool);
    new_doc: "Wrap an existing sqlx `SQLite` pool.";
    pool_doc: "Borrow the underlying sqlx pool (the unaudited raw escape hatch).";
    /// A `SQLite` transaction handle.
    struct SqliteTxConn(database: Sqlite);
    dialect: SqliteDialect;
    arguments: SqliteArguments;
}

#[allow(clippy::needless_pass_by_value)]
fn bind_scalar(args: &mut SqliteArguments, value: Value) -> Result<()> {
    let result = match value {
        Value::F32(x) => args.add(f64::from(x)),
        Value::Vector(x) => args.add(crate::vector::to_literal(&x)),
        // No `PostGIS` on SQLite; bind the WKT as plain text.
        Value::Geo { wkt, .. } => args.add(wkt),
        // A `Vec<T>` column has no native SQLite array type — store it as JSON text.
        // (`any_of` list membership never lands here; it expands to `IN (?, …)`.)
        Value::Array(_, items) => args.add(crate::value::array_to_json_text(&items)),
        scalar => add_scalar!(args, scalar),
    };
    result.map_err(Error::Encode)
}

fn bind_null(args: &mut SqliteArguments, kind: ValueKind) -> Result<()> {
    let result = match kind {
        ValueKind::F32 => args.add(None::<f64>),
        // Vector/geometry bind as text literals, and arrays store as JSON text — so
        // their nulls are text nulls too.
        ValueKind::Vector | ValueKind::Geo | ValueKind::Array(_) => args.add(None::<String>),
        scalar => add_null_scalar!(args, scalar),
    };
    result.map_err(Error::Encode)
}
