//! The Postgres backend: [`Row`] for `PgRow`, the [`PostgresDriver`], its
//! transaction handle, and `Value` → `PgArguments` binding.

use super::{BoxRow, Driver, Row, RowSink, TxConn};
use crate::dialect::{Dialect, PostgresDialect};
use crate::error::{Error, Result};
use crate::sql::BindBuffer;
use crate::value::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc, Uuid, Value, ValueKind};
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use sqlx::postgres::{PgArguments, PgRow};
use sqlx::{Arguments, PgPool, Postgres, Transaction};
use sqlx::{Row as _, ValueRef};

/// Read a nullable cell as `T`, mapping `Some`/`None` through `wrap`/the kind's
/// null. Keeps each [`ValueKind`] arm to one line.
macro_rules! read {
    ($row:expr, $index:expr, $kind:expr, $ty:ty, $wrap:expr) => {{
        let cell: Option<$ty> = $row.try_get($index).map_err(into_decode)?;
        Ok(cell.map_or(Value::Null($kind), $wrap))
    }};
}

impl Row for PgRow {
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
            ValueKind::Vector => read_vector(self, index),
            ValueKind::Geo => read_geo(self, index),
        }
    }

    fn is_null(&self, index: usize) -> Result<bool> {
        Ok(self.try_get_raw(index).map_err(into_decode)?.is_null())
    }
}

/// The Postgres [`Driver`], wrapping an sqlx pool.
#[derive(Clone)]
pub struct PostgresDriver {
    pool: PgPool,
}

impl PostgresDriver {
    /// Wrap an existing sqlx pool.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Borrow the underlying sqlx pool (the unaudited raw escape hatch, used by
    /// the migrator).
    #[must_use]
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }
}

impl Driver for PostgresDriver {
    fn dialect(&self) -> &'static dyn Dialect {
        &PostgresDialect
    }

    fn fetch<'a>(
        &'a self,
        sql: String,
        binds: BindBuffer,
        sink: &'a mut dyn RowSink,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let args = pg_args(binds)?;
            let mut rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), args).fetch(&self.pool);
            while let Some(row) = futures::TryStreamExt::try_next(&mut rows).await? {
                sink.push(&row)?;
            }
            Ok(())
        })
    }

    fn execute(&self, sql: String, binds: BindBuffer) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move {
            let args = pg_args(binds)?;
            let result = sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
                .execute(&self.pool)
                .await?;
            Ok(result.rows_affected())
        })
    }

    fn stream(&self, sql: String, binds: BindBuffer) -> BoxStream<'_, Result<BoxRow>> {
        Box::pin(async_stream::try_stream! {
            let args = pg_args(binds)?;
            let mut rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), args).fetch(&self.pool);
            while let Some(row) = futures::TryStreamExt::try_next(&mut rows).await? {
                yield Box::new(row) as BoxRow;
            }
        })
    }

    fn begin(&self) -> BoxFuture<'_, Result<Box<dyn TxConn>>> {
        Box::pin(async move {
            let transaction = self.pool.begin().await?;
            Ok(Box::new(PgTxConn { transaction }) as Box<dyn TxConn>)
        })
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// A Postgres transaction handle.
struct PgTxConn {
    transaction: Transaction<'static, Postgres>,
}

impl TxConn for PgTxConn {
    fn fetch<'a>(
        &'a mut self,
        sql: String,
        binds: BindBuffer,
        sink: &'a mut dyn RowSink,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let args = pg_args(binds)?;
            let mut rows =
                sqlx::query_with(sqlx::AssertSqlSafe(sql), args).fetch(&mut *self.transaction);
            while let Some(row) = futures::TryStreamExt::try_next(&mut rows).await? {
                sink.push(&row)?;
            }
            Ok(())
        })
    }

    fn execute(&mut self, sql: String, binds: BindBuffer) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move {
            let args = pg_args(binds)?;
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

/// Convert backend-neutral binds into Postgres arguments.
fn pg_args(binds: BindBuffer) -> Result<PgArguments> {
    let mut args = PgArguments::default();
    for value in binds {
        bind_scalar(&mut args, value)?;
    }
    Ok(args)
}

#[allow(clippy::needless_pass_by_value)]
fn bind_scalar(args: &mut PgArguments, value: Value) -> Result<()> {
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
        // Bound as the text literal `[..]`; the SQL writer adds the `::vector` cast.
        Value::Vector(x) => args.add(crate::vector::to_literal(&x)),
        // Bound as bare WKT text; the SQL writer adds the `::geometry` cast (and
        // wraps it in `ST_SetSRID(.., srid)` when a SRID is attached).
        Value::Geo { wkt, .. } => args.add(wkt),
        Value::Array(kind, values) => return bind_array(args, kind, values),
    };
    result.map_err(Error::Encode)
}

fn bind_null(args: &mut PgArguments, kind: ValueKind) -> Result<()> {
    let result = match kind {
        ValueKind::I16 => args.add(None::<i16>),
        ValueKind::I32 => args.add(None::<i32>),
        ValueKind::I64 => args.add(None::<i64>),
        ValueKind::F32 => args.add(None::<f32>),
        ValueKind::F64 => args.add(None::<f64>),
        ValueKind::Bool => args.add(None::<bool>),
        // Vector and geometry bind as text literals, so their nulls are text nulls too.
        ValueKind::Text | ValueKind::Vector | ValueKind::Geo => args.add(None::<String>),
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

/// Read a pgvector column (text form `[..]`) into a [`Value::Vector`]. The query
/// must select it as text (e.g. `embedding::text`) for this to decode.
fn read_vector(row: &PgRow, index: usize) -> Result<Value> {
    let cell: Option<String> = row.try_get(index).map_err(into_decode)?;
    match cell {
        Some(text) => Ok(Value::Vector(crate::vector::parse_literal(&text)?)),
        None => Ok(Value::Null(ValueKind::Vector)),
    }
}

/// Read a PostGIS geometry into a [`Value::Geo`]. The query must select it as text
/// (e.g. `ST_AsText(location)`), which yields the bare WKT body without an SRID.
fn read_geo(row: &PgRow, index: usize) -> Result<Value> {
    let cell: Option<String> = row.try_get(index).map_err(into_decode)?;
    Ok(cell.map_or(Value::Null(ValueKind::Geo), |wkt| Value::Geo {
        wkt,
        srid: None,
    }))
}

/// Collect homogeneous scalar `values` of `kind` into a typed `Vec` and bind it
/// as a Postgres array (the `= ANY($1)` parameter).
fn bind_array(args: &mut PgArguments, kind: ValueKind, values: Vec<Value>) -> Result<()> {
    macro_rules! collect_add {
        ($variant:ident) => {{
            let typed: Vec<_> = values
                .into_iter()
                .filter_map(|value| match value {
                    Value::$variant(inner) => Some(inner),
                    _ => None,
                })
                .collect();
            args.add(typed)
        }};
    }
    let result = match kind {
        ValueKind::I16 => collect_add!(I16),
        ValueKind::I32 => collect_add!(I32),
        ValueKind::I64 => collect_add!(I64),
        ValueKind::F32 => collect_add!(F32),
        ValueKind::F64 => collect_add!(F64),
        ValueKind::Bool => collect_add!(Bool),
        ValueKind::Text => collect_add!(Text),
        ValueKind::Bytes => collect_add!(Bytes),
        ValueKind::Uuid => collect_add!(Uuid),
        ValueKind::Timestamptz => collect_add!(Timestamptz),
        ValueKind::NaiveDateTime => collect_add!(NaiveDateTime),
        ValueKind::Date => collect_add!(Date),
        ValueKind::NaiveTime => collect_add!(NaiveTime),
        ValueKind::Json => {
            return Err(Error::Encode("json array binds are not supported".into()));
        }
        ValueKind::Vector => {
            return Err(Error::Encode("vector array binds are not supported".into()));
        }
        ValueKind::Geo => {
            return Err(Error::Encode("geometry array binds are not supported".into()));
        }
    };
    result.map_err(Error::Encode)
}
