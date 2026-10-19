//! The `MySQL` backend (sqlx): [`Row`] for `MySqlRow`, the [`MySqlDriver`], its
//! transaction handle, and `Value` → `MySqlArguments` binding.
//!
//! `MySQL` has no `RETURNING` clause, so `insert(...).returning(...)` is not
//! supported on this backend; use a follow-up `select`. Arrays are unsupported
//! (list membership is expanded to `IN (?, …)` by the dialect).

use super::sqlx_backend::{
    AddResult, add_null_scalar, add_scalar, array_from_json_text, read_scalar, sqlx_driver,
    text_cell,
};
use crate::dialect::MySqlDialect;
use crate::error::{Error, Result};
use crate::value::{Value, ValueKind};
use sqlx::mysql::{MySqlArguments, MySqlPool, MySqlRow};
use sqlx::{Arguments, MySql, Row as _};

fn try_value(row: &MySqlRow, index: usize, kind: ValueKind) -> Result<Value> {
    match kind {
        ValueKind::Vector => Err(Error::Decode("MySQL has no vector type".into())),
        ValueKind::Geo => Err(Error::Decode("PostGIS geometry is Postgres-only".into())),
        // No native arrays on MySQL — stored as JSON text.
        ValueKind::Array(elem) => array_from_json_text(text_cell(row, index)?, kind, *elem),
        _ => read_scalar!(row, index, kind),
    }
}

sqlx_driver! {
    row: MySqlRow;
    /// The `MySQL` [`Driver`](super::Driver), wrapping an sqlx pool.
    pub struct MySqlDriver(pool: MySqlPool);
    new_doc: "Wrap an existing sqlx `MySQL` pool.";
    pool_doc: "Borrow the underlying sqlx pool (the unaudited raw escape hatch).";
    /// A `MySQL` transaction handle.
    struct MySqlTxConn(database: MySql);
    dialect: MySqlDialect;
    arguments: MySqlArguments;
}

#[allow(clippy::needless_pass_by_value)]
fn bind_scalar(args: &mut MySqlArguments, value: Value) -> AddResult {
    match value {
        Value::Vector(..) => Err("MySQL has no vector type".into()),
        Value::Geo { .. } => Err("PostGIS geometry is Postgres-only".into()),
        // A `Vec<T>` column has no native MySQL array type — store it as JSON text.
        Value::Array(_, items) => args.add(crate::value::array_to_json_text(&items)),
        scalar => add_scalar!(args, scalar),
    }
}

fn bind_null(args: &mut MySqlArguments, kind: ValueKind) -> AddResult {
    match kind {
        // Arrays (stored as JSON text) bind a text null.
        ValueKind::Array(_) => args.add(None::<String>),
        ValueKind::Vector => Err("MySQL has no vector type".into()),
        ValueKind::Geo => Err("PostGIS geometry is Postgres-only".into()),
        scalar => add_null_scalar!(args, scalar),
    }
}
