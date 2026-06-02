//! The `MySQL` backend (sqlx): [`Row`] for `MySqlRow`, the [`MySqlDriver`], its
//! transaction handle, and `Value` → `MySqlArguments` binding.
//!
//! `MySQL` has no `RETURNING` clause, so `insert(...).returning(...)` is not
//! supported on this backend; use a follow-up `select`. Arrays are unsupported
//! (list membership is expanded to `IN (?, …)` by the dialect).

use super::{BoxRow, Driver, Row, RowSink, TxConn};
use crate::dialect::{Dialect, MySqlDialect};
use crate::error::{Error, Result};
use crate::sql::BindBuffer;
use crate::value::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc, Uuid, Value, ValueKind};
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use sqlx::mysql::{MySqlArguments, MySqlConnection, MySqlPool, MySqlRow};
use sqlx::{Arguments, MySql, Row as _, Transaction, ValueRef};

/// Read a nullable cell as `T`, mapping `Some`/`None` through `wrap`/the kind's
/// null.
macro_rules! read {
    ($row:expr, $index:expr, $kind:expr, $ty:ty, $wrap:expr) => {{
        let cell: Option<$ty> = $row.try_get($index).map_err(into_decode)?;
        Ok(cell.map_or(Value::Null($kind), $wrap))
    }};
}

impl Row for MySqlRow {
    fn try_value(&self, index: usize, kind: ValueKind) -> Result<Value> {
        match kind {
            ValueKind::I16 => read!(self, index, kind, i16, Value::I16),
            ValueKind::I32 => read!(self, index, kind, i32, Value::I32),
            ValueKind::I64 => read!(self, index, kind, i64, Value::I64),
            ValueKind::F32 => read!(self, index, kind, f32, Value::F32),
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
        }
    }

    fn is_null(&self, index: usize) -> Result<bool> {
        Ok(self.try_get_raw(index).map_err(into_decode)?.is_null())
    }
}

/// The `MySQL` [`Driver`], wrapping an sqlx pool.
#[derive(Clone)]
pub struct MySqlDriver {
    pool: MySqlPool,
}

impl MySqlDriver {
    /// Wrap an existing sqlx `MySQL` pool.
    #[must_use]
    pub const fn new(pool: MySqlPool) -> Self {
        Self { pool }
    }

    /// Borrow the underlying sqlx pool (the unaudited raw escape hatch).
    #[must_use]
    pub const fn pool(&self) -> &MySqlPool {
        &self.pool
    }
}

impl Driver for MySqlDriver {
    fn dialect(&self) -> &'static dyn Dialect {
        &MySqlDialect
    }

    fn fetch<'a>(
        &'a self,
        sql: String,
        binds: BindBuffer,
        sink: &'a mut dyn RowSink,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let args = mysql_args(binds)?;
            let mut rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), args).fetch(&self.pool);
            while let Some(row) = futures::TryStreamExt::try_next(&mut rows).await? {
                sink.push(&row)?;
            }
            Ok(())
        })
    }

    fn execute(&self, sql: String, binds: BindBuffer) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move {
            let args = mysql_args(binds)?;
            let result = sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
                .execute(&self.pool)
                .await?;
            Ok(result.rows_affected())
        })
    }

    fn stream(&self, sql: String, binds: BindBuffer) -> BoxStream<'_, Result<BoxRow>> {
        Box::pin(async_stream::try_stream! {
            let args = mysql_args(binds)?;
            let mut rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), args).fetch(&self.pool);
            while let Some(row) = futures::TryStreamExt::try_next(&mut rows).await? {
                yield Box::new(row) as BoxRow;
            }
        })
    }

    fn begin(&self) -> BoxFuture<'_, Result<Box<dyn TxConn>>> {
        Box::pin(async move {
            let transaction = self.pool.begin().await?;
            Ok(Box::new(MySqlTxConn { transaction }) as Box<dyn TxConn>)
        })
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// A `MySQL` transaction handle.
struct MySqlTxConn {
    transaction: Transaction<'static, MySql>,
}

impl TxConn for MySqlTxConn {
    fn fetch<'a>(
        &'a mut self,
        sql: String,
        binds: BindBuffer,
        sink: &'a mut dyn RowSink,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let args = mysql_args(binds)?;
            let connection: &mut MySqlConnection = &mut self.transaction;
            let mut rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), args).fetch(connection);
            while let Some(row) = futures::TryStreamExt::try_next(&mut rows).await? {
                sink.push(&row)?;
            }
            Ok(())
        })
    }

    fn execute(&mut self, sql: String, binds: BindBuffer) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move {
            let args = mysql_args(binds)?;
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

/// Convert backend-neutral binds into `MySQL` arguments.
fn mysql_args(binds: BindBuffer) -> Result<MySqlArguments> {
    let mut args = MySqlArguments::default();
    for value in binds {
        bind_scalar(&mut args, value)?;
    }
    Ok(args)
}

#[allow(clippy::needless_pass_by_value)]
fn bind_scalar(args: &mut MySqlArguments, value: Value) -> Result<()> {
    let result = match value {
        Value::Null(kind) => return bind_null(args, kind),
        Value::I16(x) => args.add(x),
        Value::I32(x) => args.add(x),
        Value::I64(x) => args.add(x),
        Value::F32(x) => args.add(x),
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
        Value::Array(..) => {
            return Err(Error::Encode("MySQL does not support array binds".into()));
        }
    };
    result.map_err(Error::Encode)
}

fn bind_null(args: &mut MySqlArguments, kind: ValueKind) -> Result<()> {
    let result = match kind {
        ValueKind::I16 => args.add(None::<i16>),
        ValueKind::I32 => args.add(None::<i32>),
        ValueKind::I64 => args.add(None::<i64>),
        ValueKind::F32 => args.add(None::<f32>),
        ValueKind::F64 => args.add(None::<f64>),
        ValueKind::Bool => args.add(None::<bool>),
        ValueKind::Text => args.add(None::<String>),
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
