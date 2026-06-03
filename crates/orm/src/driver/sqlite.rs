//! The `SQLite` backend (sqlx): [`Row`] for `SqliteRow`, the [`SqliteDriver`], its
//! transaction handle, and `Value` → `SqliteArguments` binding.
//!
//! `SQLite` stores a narrow set of native types, so a few [`Value`] kinds map onto
//! wider storage: `f32` round-trips through `REAL` (`f64`), and arrays are
//! unsupported (list membership is expanded to `IN (?, …)` by the dialect, so the
//! array bind path is never hit for `SQLite`).

use super::{BoxRow, Driver, Row, RowSink, TxConn};
use crate::dialect::{Dialect, SqliteDialect};
use crate::error::{Error, Result};
use crate::sql::BindBuffer;
use crate::value::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc, Uuid, Value, ValueKind};
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use sqlx::sqlite::{SqliteArguments, SqliteConnection};
use sqlx::sqlite::{SqlitePool, SqliteRow};
use sqlx::{Arguments, Row as _, Sqlite, Transaction, ValueRef};

/// Read a nullable cell as `T`, mapping `Some`/`None` through `wrap`/the kind's
/// null.
macro_rules! read {
    ($row:expr, $index:expr, $kind:expr, $ty:ty, $wrap:expr) => {{
        let cell: Option<$ty> = $row.try_get($index).map_err(into_decode)?;
        Ok(cell.map_or(Value::Null($kind), $wrap))
    }};
}

impl Row for SqliteRow {
    fn try_value(&self, index: usize, kind: ValueKind) -> Result<Value> {
        match kind {
            ValueKind::I16 => read!(self, index, kind, i16, Value::I16),
            ValueKind::I32 => read!(self, index, kind, i32, Value::I32),
            ValueKind::I64 => read!(self, index, kind, i64, Value::I64),
            // SQLite has no 32-bit float; widen through REAL.
            ValueKind::F32 => {
                let cell: Option<f64> = self.try_get(index).map_err(into_decode)?;
                #[allow(clippy::cast_possible_truncation)]
                Ok(cell.map_or(Value::Null(kind), |value| Value::F32(value as f32)))
            }
            ValueKind::F64 => read!(self, index, kind, f64, Value::F64),
            ValueKind::Bool => read!(self, index, kind, bool, Value::Bool),
            ValueKind::Text => read!(self, index, kind, String, Value::Text),
            ValueKind::Bytes => read!(self, index, kind, Vec<u8>, Value::Bytes),
            ValueKind::Uuid => read!(self, index, kind, Uuid, Value::Uuid),
            ValueKind::Timestamptz => {
                read!(self, index, kind, DateTime<Utc>, Value::Timestamptz)
            }
            ValueKind::NaiveDateTime => {
                read!(self, index, kind, NaiveDateTime, Value::NaiveDateTime)
            }
            ValueKind::Date => read!(self, index, kind, NaiveDate, Value::Date),
            ValueKind::NaiveTime => read!(self, index, kind, NaiveTime, Value::NaiveTime),
            ValueKind::Json => read!(self, index, kind, serde_json::Value, Value::Json),
            ValueKind::Vector => {
                let cell: Option<String> = self.try_get(index).map_err(into_decode)?;
                match cell {
                    Some(text) => Ok(Value::Vector(crate::vector::parse_literal(&text)?)),
                    None => Ok(Value::Null(kind)),
                }
            }
            // SQLite has no `PostGIS`; the WKT round-trips as plain text.
            ValueKind::Geo => {
                let cell: Option<String> = self.try_get(index).map_err(into_decode)?;
                Ok(cell.map_or(Value::Null(kind), |wkt| Value::Geo { wkt, srid: None }))
            }
            // No native arrays on SQLite — stored as JSON text.
            ValueKind::Array(elem) => {
                let cell: Option<String> = self.try_get(index).map_err(into_decode)?;
                cell.map_or_else(
                    || Ok(Value::Null(kind)),
                    |text| crate::value::json_text_to_array(&text, *elem),
                )
            }
        }
    }

    fn is_null(&self, index: usize) -> Result<bool> {
        Ok(self.try_get_raw(index).map_err(into_decode)?.is_null())
    }
}

/// The `SQLite` [`Driver`], wrapping an sqlx pool.
#[derive(Clone)]
pub struct SqliteDriver {
    pool: SqlitePool,
}

impl SqliteDriver {
    /// Wrap an existing sqlx `SQLite` pool.
    #[must_use]
    pub const fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Borrow the underlying sqlx pool (the unaudited raw escape hatch).
    #[must_use]
    pub const fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

impl Driver for SqliteDriver {
    fn dialect(&self) -> &'static dyn Dialect {
        &SqliteDialect
    }

    fn fetch<'a>(
        &'a self,
        sql: String,
        binds: BindBuffer,
        sink: &'a mut dyn RowSink,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let args = sqlite_args(binds)?;
            let mut rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), args).fetch(&self.pool);
            while let Some(row) = futures::TryStreamExt::try_next(&mut rows).await? {
                sink.push(&row)?;
            }
            Ok(())
        })
    }

    fn execute(&self, sql: String, binds: BindBuffer) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move {
            let args = sqlite_args(binds)?;
            let result = sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
                .execute(&self.pool)
                .await?;
            Ok(result.rows_affected())
        })
    }

    fn stream(&self, sql: String, binds: BindBuffer) -> BoxStream<'_, Result<BoxRow>> {
        Box::pin(async_stream::try_stream! {
            let args = sqlite_args(binds)?;
            let mut rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), args).fetch(&self.pool);
            while let Some(row) = futures::TryStreamExt::try_next(&mut rows).await? {
                yield Box::new(row) as BoxRow;
            }
        })
    }

    fn begin(&self) -> BoxFuture<'_, Result<Box<dyn TxConn>>> {
        Box::pin(async move {
            let transaction = self.pool.begin().await?;
            Ok(Box::new(SqliteTxConn { transaction }) as Box<dyn TxConn>)
        })
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// A `SQLite` transaction handle.
struct SqliteTxConn {
    transaction: Transaction<'static, Sqlite>,
}

impl TxConn for SqliteTxConn {
    fn fetch<'a>(
        &'a mut self,
        sql: String,
        binds: BindBuffer,
        sink: &'a mut dyn RowSink,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let args = sqlite_args(binds)?;
            let connection: &mut SqliteConnection = &mut self.transaction;
            let mut rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), args).fetch(connection);
            while let Some(row) = futures::TryStreamExt::try_next(&mut rows).await? {
                sink.push(&row)?;
            }
            Ok(())
        })
    }

    fn execute(&mut self, sql: String, binds: BindBuffer) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move {
            let args = sqlite_args(binds)?;
            let result = sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
                .execute(&mut *self.transaction)
                .await?;
            Ok(result.rows_affected())
        })
    }

    fn commit(self: Box<Self>) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            self.transaction.commit().await?;
            Ok(())
        })
    }

    fn rollback(self: Box<Self>) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            self.transaction.rollback().await?;
            Ok(())
        })
    }
}

/// Wrap an sqlx decode error as [`Error::Decode`].
fn into_decode(error: sqlx::Error) -> Error {
    Error::Decode(Box::new(error))
}

/// Convert backend-neutral binds into `SQLite` arguments.
fn sqlite_args(binds: BindBuffer) -> Result<SqliteArguments> {
    let mut args = SqliteArguments::default();
    for value in binds {
        bind_scalar(&mut args, value)?;
    }
    Ok(args)
}

#[allow(clippy::needless_pass_by_value)]
fn bind_scalar(args: &mut SqliteArguments, value: Value) -> Result<()> {
    let result = match value {
        Value::Null(kind) => return bind_null(args, kind),
        Value::I16(x) => args.add(x),
        Value::I32(x) => args.add(x),
        Value::I64(x) => args.add(x),
        Value::F32(x) => args.add(f64::from(x)),
        Value::F64(x) => args.add(x),
        Value::Bool(x) => args.add(x),
        Value::Text(x) => args.add(x),
        Value::Bytes(x) => args.add(x),
        Value::Uuid(x) => args.add(x),
        Value::Timestamptz(x) => args.add(x),
        Value::NaiveDateTime(x) => args.add(x),
        Value::Date(x) => args.add(x),
        Value::NaiveTime(x) => args.add(x),
        Value::Json(x) => args.add(x),
        Value::Vector(x) => args.add(crate::vector::to_literal(&x)),
        // No `PostGIS` on SQLite; bind the WKT as plain text.
        Value::Geo { wkt, .. } => args.add(wkt),
        // A `Vec<T>` column has no native SQLite array type — store it as JSON text.
        // (`any_of` list membership never lands here; it expands to `IN (?, …)`.)
        Value::Array(_, items) => args.add(crate::value::array_to_json_text(&items)),
        // `::` casts aren't a SQLite thing; the SQL writer already flagged this bind
        // as unsupported (the statement errors before reaching here). Defensive only.
        Value::Cast { inner, .. } => return bind_scalar(args, *inner),
    };
    result.map_err(Error::Encode)
}

fn bind_null(args: &mut SqliteArguments, kind: ValueKind) -> Result<()> {
    let result = match kind {
        ValueKind::I16 => args.add(None::<i16>),
        ValueKind::I32 => args.add(None::<i32>),
        ValueKind::I64 => args.add(None::<i64>),
        ValueKind::F32 | ValueKind::F64 => args.add(None::<f64>),
        ValueKind::Bool => args.add(None::<bool>),
        // Vector/geometry bind as text literals, and arrays store as JSON text — so
        // their nulls are text nulls too.
        ValueKind::Text | ValueKind::Vector | ValueKind::Geo | ValueKind::Array(_) => {
            args.add(None::<String>)
        }
        ValueKind::Bytes => args.add(None::<Vec<u8>>),
        ValueKind::Uuid => args.add(None::<Uuid>),
        ValueKind::Timestamptz => args.add(None::<DateTime<Utc>>),
        ValueKind::NaiveDateTime => args.add(None::<NaiveDateTime>),
        ValueKind::Date => args.add(None::<NaiveDate>),
        ValueKind::NaiveTime => args.add(None::<NaiveTime>),
        ValueKind::Json => args.add(None::<serde_json::Value>),
    };
    result.map_err(Error::Encode)
}
