//! The Turso / `libSQL` backend (the non-sqlx driver — the real test of the
//! [`Driver`] abstraction): [`Row`] for `libsql::Row`, the [`TursoDriver`], its
//! transaction handle, and `Value` ⇄ `libsql::Value` conversion.
//!
//! `libSQL` stores `SQLite`'s five native types (NULL / INTEGER / REAL / TEXT /
//! BLOB), so richer [`Value`] kinds map through them: `Uuid`/`Timestamptz`/`Date`
//! round-trip as TEXT, `bool` as INTEGER. Arrays are unsupported (the dialect
//! expands `any_of` to `IN (?, …)`).

use super::{BoxRow, Driver, Row, RowSink, TxConn};
use crate::dialect::{Dialect, TursoDialect};
use crate::error::{Error, Result};
use crate::sql::BindBuffer;
use crate::value::{DateTime, NaiveDate, Utc, Uuid, Value, ValueKind};
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use libsql::{Connection, Value as LibsqlValue};

impl Row for libsql::Row {
    fn try_value(&self, index: usize, kind: ValueKind) -> Result<Value> {
        let column = column_index(index)?;
        let cell = self.get_value(column).map_err(map_libsql)?;
        cell_to_value(cell, kind)
    }

    fn is_null(&self, index: usize) -> Result<bool> {
        let column = column_index(index)?;
        let cell = self.get_value(column).map_err(map_libsql)?;
        Ok(matches!(cell, LibsqlValue::Null))
    }
}

/// The Turso / `libSQL` [`Driver`], wrapping a (cheaply cloneable) connection.
#[derive(Clone)]
pub struct TursoDriver {
    connection: Connection,
}

impl TursoDriver {
    /// Wrap an existing `libSQL` [`Connection`].
    #[must_use]
    pub const fn new(connection: Connection) -> Self {
        Self { connection }
    }

    /// Borrow the underlying `libSQL` connection (the raw escape hatch).
    #[must_use]
    pub const fn connection(&self) -> &Connection {
        &self.connection
    }
}

impl Driver for TursoDriver {
    fn dialect(&self) -> &'static dyn Dialect {
        &TursoDialect
    }

    fn fetch<'a>(
        &'a self,
        sql: String,
        binds: BindBuffer,
        sink: &'a mut dyn RowSink,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let params = libsql_params(binds)?;
            let mut rows = self
                .connection
                .query(&sql, params)
                .await
                .map_err(map_libsql)?;
            while let Some(row) = rows.next().await.map_err(map_libsql)? {
                sink.push(&row)?;
            }
            Ok(())
        })
    }

    fn execute(&self, sql: String, binds: BindBuffer) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move {
            let params = libsql_params(binds)?;
            let affected = self
                .connection
                .execute(&sql, params)
                .await
                .map_err(map_libsql)?;
            Ok(affected)
        })
    }

    fn stream(&self, sql: String, binds: BindBuffer) -> BoxStream<'_, Result<BoxRow>> {
        Box::pin(async_stream::try_stream! {
            let params = libsql_params(binds)?;
            let mut rows = self.connection.query(&sql, params).await.map_err(map_libsql)?;
            while let Some(row) = rows.next().await.map_err(map_libsql)? {
                yield Box::new(row) as BoxRow;
            }
        })
    }

    fn begin(&self) -> BoxFuture<'_, Result<Box<dyn TxConn>>> {
        Box::pin(async move {
            let transaction = self.connection.transaction().await.map_err(map_libsql)?;
            Ok(Box::new(TursoTxConn { transaction }) as Box<dyn TxConn>)
        })
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// A Turso / `libSQL` transaction handle.
struct TursoTxConn {
    transaction: libsql::Transaction,
}

impl TxConn for TursoTxConn {
    fn fetch<'a>(
        &'a mut self,
        sql: String,
        binds: BindBuffer,
        sink: &'a mut dyn RowSink,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let params = libsql_params(binds)?;
            let mut rows = self
                .transaction
                .query(&sql, params)
                .await
                .map_err(map_libsql)?;
            while let Some(row) = rows.next().await.map_err(map_libsql)? {
                sink.push(&row)?;
            }
            Ok(())
        })
    }

    fn execute(&mut self, sql: String, binds: BindBuffer) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move {
            let params = libsql_params(binds)?;
            let affected = self
                .transaction
                .execute(&sql, params)
                .await
                .map_err(map_libsql)?;
            Ok(affected)
        })
    }

    fn commit(self: Box<Self>) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            self.transaction.commit().await.map_err(map_libsql)?;
            Ok(())
        })
    }

    fn rollback(self: Box<Self>) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            self.transaction.rollback().await.map_err(map_libsql)?;
            Ok(())
        })
    }
}

/// `libSQL` indexes columns with an `i32`.
fn column_index(index: usize) -> Result<i32> {
    i32::try_from(index).map_err(|_| Error::Decode("column index out of range".into()))
}

/// Map any `libSQL` error to a typed [`Error`]. Constraint violations are
/// classified from the `SQLite` extended result code (so `is_unique()` etc. work on
/// Turso, matching the sqlx backends); everything else keeps the concrete
/// `libsql::Error` in [`Error::Turso`] (not boxed).
fn map_libsql(error: libsql::Error) -> Error {
    // SQLite extended result codes for constraint violations.
    const UNIQUE: i32 = 2067;
    const PRIMARY_KEY: i32 = 1555;
    const FOREIGN_KEY: i32 = 787;
    const NOT_NULL: i32 = 1299;
    const CHECK: i32 = 275;

    let code = match &error {
        libsql::Error::SqliteFailure(code, _) => Some(*code),
        libsql::Error::RemoteSqliteFailure(_, extended, _) => Some(*extended),
        _ => None,
    };
    match code {
        Some(UNIQUE | PRIMARY_KEY) => Error::Unique {
            constraint: String::new(),
        },
        Some(FOREIGN_KEY) => Error::ForeignKey {
            constraint: String::new(),
        },
        Some(NOT_NULL) => Error::NotNull {
            column: String::new(),
        },
        Some(CHECK) => Error::Check {
            constraint: String::new(),
        },
        _ => Error::Turso(error),
    }
}

/// Build positional `libSQL` params from backend-neutral binds.
fn libsql_params(binds: BindBuffer) -> Result<libsql::params::Params> {
    let mut values = Vec::with_capacity(binds.len());
    for value in binds {
        values.push(to_libsql(value)?);
    }
    Ok(libsql::params::Params::Positional(values))
}

#[allow(clippy::needless_pass_by_value)]
fn to_libsql(value: Value) -> Result<LibsqlValue> {
    Ok(match value {
        Value::Null(_) => LibsqlValue::Null,
        Value::I16(x) => LibsqlValue::Integer(x.into()),
        Value::I32(x) => LibsqlValue::Integer(x.into()),
        Value::I64(x) => LibsqlValue::Integer(x),
        Value::F32(x) => LibsqlValue::Real(x.into()),
        Value::F64(x) => LibsqlValue::Real(x),
        Value::Bool(x) => LibsqlValue::Integer(x.into()),
        Value::Text(x) => LibsqlValue::Text(x),
        Value::Bytes(x) => LibsqlValue::Blob(x),
        Value::Uuid(x) => LibsqlValue::Text(x.to_string()),
        Value::Timestamptz(x) => LibsqlValue::Text(x.to_rfc3339()),
        Value::Date(x) => LibsqlValue::Text(x.to_string()),
        Value::Array(..) => {
            return Err(Error::Encode("Turso does not support array binds".into()));
        }
    })
}

/// Convert a `libSQL` cell into a backend-neutral [`Value`] of `kind`.
fn cell_to_value(cell: LibsqlValue, kind: ValueKind) -> Result<Value> {
    if matches!(cell, LibsqlValue::Null) {
        return Ok(Value::Null(kind));
    }
    Ok(match kind {
        // libSQL stores all integers as i64; narrow with a checked conversion so an
        // out-of-range value errors instead of silently wrapping (data corruption).
        ValueKind::I16 => Value::I16(narrow(as_int(&cell)?, "i16")?),
        ValueKind::I32 => Value::I32(narrow(as_int(&cell)?, "i32")?),
        ValueKind::I64 => Value::I64(as_int(&cell)?),
        #[allow(clippy::cast_possible_truncation)]
        ValueKind::F32 => Value::F32(as_real(&cell)? as f32),
        ValueKind::F64 => Value::F64(as_real(&cell)?),
        ValueKind::Bool => Value::Bool(as_int(&cell)? != 0),
        ValueKind::Text => Value::Text(as_text(cell)?),
        ValueKind::Bytes => Value::Bytes(as_blob(cell)?),
        ValueKind::Uuid => {
            let text = as_text(cell)?;
            Uuid::parse_str(&text)
                .map(Value::Uuid)
                .map_err(|error| Error::Decode(Box::new(error)))?
        }
        ValueKind::Timestamptz => {
            let text = as_text(cell)?;
            DateTime::parse_from_rfc3339(&text)
                .map(|dt| Value::Timestamptz(dt.with_timezone(&Utc)))
                .map_err(|error| Error::Decode(Box::new(error)))?
        }
        ValueKind::Date => {
            let text = as_text(cell)?;
            text.parse::<NaiveDate>()
                .map(Value::Date)
                .map_err(|error| Error::Decode(Box::new(error)))?
        }
    })
}

/// Checked narrowing of a `libSQL` `i64` cell into a smaller integer type.
fn narrow<T>(value: i64, target: &str) -> Result<T>
where
    T: TryFrom<i64>,
{
    T::try_from(value)
        .map_err(|_| Error::Decode(format!("integer {value} out of range for {target}").into()))
}

fn type_error(expected: &str) -> Error {
    Error::Decode(format!("expected {expected} from libSQL cell").into())
}

fn as_int(cell: &LibsqlValue) -> Result<i64> {
    match cell {
        LibsqlValue::Integer(x) => Ok(*x),
        _ => Err(type_error("integer")),
    }
}

#[allow(clippy::cast_precision_loss)]
fn as_real(cell: &LibsqlValue) -> Result<f64> {
    match cell {
        LibsqlValue::Real(x) => Ok(*x),
        LibsqlValue::Integer(x) => Ok(*x as f64),
        _ => Err(type_error("real")),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn as_text(cell: LibsqlValue) -> Result<String> {
    match cell {
        LibsqlValue::Text(x) => Ok(x),
        _ => Err(type_error("text")),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn as_blob(cell: LibsqlValue) -> Result<Vec<u8>> {
    match cell {
        LibsqlValue::Blob(x) => Ok(x),
        _ => Err(type_error("blob")),
    }
}
