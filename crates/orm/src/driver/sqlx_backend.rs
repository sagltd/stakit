//! Code shared by the sqlx-backed drivers (Postgres, `SQLite`, `MySQL`).
//!
//! The three backends differ only in their sqlx types and in how a handful of
//! [`Value`](crate::value::Value) kinds (vectors, geometry, arrays, `f32` on
//! `SQLite`) map onto storage. Everything else — the scalar decode/bind arms and
//! the pool/transaction plumbing — is generated here once per backend. Macros
//! (rather than a generic `SqlxDriver<DB>`) keep each driver a concrete public
//! type and avoid sqlx's executor/argument trait-bound maze.

use crate::error::{Error, Result};
use crate::value::{Value, ValueKind};

/// Wrap an sqlx decode error as [`Error::Decode`].
pub(super) fn into_decode(error: sqlx::Error) -> Error {
    Error::Decode(Box::new(error))
}

/// Read the cell at `index` as nullable text — the storage form of vectors,
/// geometry WKT, and (off Postgres) JSON-encoded arrays.
pub(super) fn text_cell<R>(row: &R, index: usize) -> Result<Option<String>>
where
    R: sqlx::Row,
    usize: sqlx::ColumnIndex<R>,
    String: for<'r> sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    row.try_get(index).map_err(into_decode)
}

/// A vector cell stored as its text literal `[..]`.
#[cfg(any(feature = "postgres", feature = "sqlite"))]
pub(super) fn vector_from_text(text: Option<String>) -> Result<Value> {
    match text {
        Some(text) => Ok(Value::Vector(crate::vector::parse_literal(&text)?)),
        None => Ok(Value::Null(ValueKind::Vector)),
    }
}

/// A geometry cell stored as bare WKT text (no SRID).
#[cfg(any(feature = "postgres", feature = "sqlite"))]
pub(super) fn geo_from_text(text: Option<String>) -> Value {
    text.map_or(Value::Null(ValueKind::Geo), |wkt| Value::Geo {
        wkt,
        srid: None,
    })
}

/// An array cell stored as JSON text (backends without native arrays). `kind` is
/// the full `ValueKind::Array(elem)` (used for `NULL`).
#[cfg(any(feature = "sqlite", feature = "mysql"))]
pub(super) fn array_from_json_text(
    text: Option<String>,
    kind: ValueKind,
    elem: ValueKind,
) -> Result<Value> {
    text.map_or_else(
        || Ok(Value::Null(kind)),
        |text| crate::value::json_text_to_array(&text, elem),
    )
}

/// Read a nullable cell as `$ty`, mapping `Some`/`None` through `$wrap`/the
/// kind's null.
macro_rules! read_cell {
    ($row:expr, $index:expr, $kind:expr, $ty:ty, $wrap:expr) => {{
        let cell: Option<$ty> = $row
            .try_get($index)
            .map_err($crate::driver::sqlx_backend::into_decode)?;
        Ok(cell.map_or($crate::value::Value::Null($kind), $wrap))
    }};
}

/// Decode a cell whose kind maps 1:1 onto an sqlx type on every backend. Callers
/// match their backend-specific kinds first and fall through to this.
macro_rules! read_scalar {
    ($row:expr, $index:expr, $kind:expr) => {{
        use $crate::driver::sqlx_backend::read_cell;
        use $crate::value::{
            DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc, Uuid, Value, ValueKind,
        };
        match $kind {
            ValueKind::I16 => read_cell!($row, $index, $kind, i16, Value::I16),
            ValueKind::I32 => read_cell!($row, $index, $kind, i32, Value::I32),
            ValueKind::I64 => read_cell!($row, $index, $kind, i64, Value::I64),
            ValueKind::F32 => read_cell!($row, $index, $kind, f32, Value::F32),
            ValueKind::F64 => read_cell!($row, $index, $kind, f64, Value::F64),
            ValueKind::Bool => read_cell!($row, $index, $kind, bool, Value::Bool),
            ValueKind::Text => read_cell!($row, $index, $kind, String, Value::Text),
            ValueKind::Bytes => read_cell!($row, $index, $kind, Vec<u8>, Value::Bytes),
            ValueKind::Uuid => read_cell!($row, $index, $kind, Uuid, Value::Uuid),
            ValueKind::Timestamptz => {
                read_cell!($row, $index, $kind, DateTime<Utc>, Value::Timestamptz)
            }
            ValueKind::NaiveDateTime => {
                read_cell!($row, $index, $kind, NaiveDateTime, Value::NaiveDateTime)
            }
            ValueKind::Date => read_cell!($row, $index, $kind, NaiveDate, Value::Date),
            ValueKind::NaiveTime => {
                read_cell!($row, $index, $kind, NaiveTime, Value::NaiveTime)
            }
            ValueKind::Json => read_cell!($row, $index, $kind, serde_json::Value, Value::Json),
            other => Err($crate::error::Error::Decode(
                format!("{other:?} has no scalar decode on this backend").into(),
            )),
        }
    }};
}

/// Bind a value whose variant maps 1:1 onto an sqlx type on every backend,
/// yielding sqlx's `add` result. Callers match their backend-specific variants
/// first and fall through to this.
macro_rules! add_scalar {
    ($args:expr, $value:expr) => {{
        use $crate::value::Value;
        match $value {
            Value::I16(x) => $args.add(x),
            Value::I32(x) => $args.add(x),
            Value::I64(x) => $args.add(x),
            Value::F32(x) => $args.add(x),
            Value::F64(x) => $args.add(x),
            Value::Bool(x) => $args.add(x),
            Value::Text(x) => $args.add(x),
            Value::Bytes(x) => $args.add(x),
            Value::Uuid(x) => $args.add(x),
            Value::Timestamptz(x) => $args.add(x),
            Value::NaiveDateTime(x) => $args.add(x),
            Value::Date(x) => $args.add(x),
            Value::NaiveTime(x) => $args.add(x),
            Value::Json(x) => $args.add(x),
            other => Err(format!("{other:?} has no scalar bind on this backend").into()),
        }
    }};
}

/// Bind a typed `NULL` for a kind that maps 1:1 onto an sqlx type on every
/// backend, yielding sqlx's `add` result. Callers match their backend-specific
/// kinds first and fall through to this.
macro_rules! add_null_scalar {
    ($args:expr, $kind:expr) => {{
        use $crate::value::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc, Uuid, ValueKind};
        match $kind {
            ValueKind::I16 => $args.add(None::<i16>),
            ValueKind::I32 => $args.add(None::<i32>),
            ValueKind::I64 => $args.add(None::<i64>),
            ValueKind::F32 => $args.add(None::<f32>),
            ValueKind::F64 => $args.add(None::<f64>),
            ValueKind::Bool => $args.add(None::<bool>),
            ValueKind::Text => $args.add(None::<String>),
            ValueKind::Bytes => $args.add(None::<Vec<u8>>),
            ValueKind::Uuid => $args.add(None::<Uuid>),
            ValueKind::Timestamptz => $args.add(None::<DateTime<Utc>>),
            ValueKind::NaiveDateTime => $args.add(None::<NaiveDateTime>),
            ValueKind::Date => $args.add(None::<NaiveDate>),
            ValueKind::NaiveTime => $args.add(None::<NaiveTime>),
            ValueKind::Json => $args.add(None::<serde_json::Value>),
            other => Err(format!("{other:?} has no scalar null bind on this backend").into()),
        }
    }};
}

/// An `async` block running a query on `$executor`, pushing each row into
/// `$sink`.
macro_rules! fetch_into {
    ($executor:expr, $sql:expr, $binds:expr, $sink:expr) => {
        async move {
            let args = sqlx_args($binds)?;
            let mut rows = sqlx::query_with(sqlx::AssertSqlSafe($sql), args).fetch($executor);
            while let Some(row) = futures::TryStreamExt::try_next(&mut rows).await? {
                $sink.push(&row)?;
            }
            Ok(())
        }
    };
}

/// An `async` block running a statement on `$executor`, yielding the
/// affected-row count.
macro_rules! execute_on {
    ($executor:expr, $sql:expr, $binds:expr) => {
        async move {
            let args = sqlx_args($binds)?;
            let result = sqlx::query_with(sqlx::AssertSqlSafe($sql), args)
                .execute($executor)
                .await?;
            Ok(result.rows_affected())
        }
    };
}

/// Generate the [`Row`](crate::driver::Row) impl (delegating to the backend's
/// own `try_value`), a pool-backed sqlx [`Driver`](crate::driver::Driver), its
/// transaction handle, and the `BindBuffer` → arguments conversion (which calls
/// the backend's own `bind_scalar` / `bind_null`).
macro_rules! sqlx_driver {
    (
        row: $row:ty;
        $(#[$driver_meta:meta])*
        pub struct $driver:ident(pool: $pool:ty);
        new_doc: $new_doc:literal;
        pool_doc: $pool_doc:literal;
        $(#[$tx_meta:meta])*
        struct $tx:ident(database: $db:ty);
        dialect: $dialect:expr;
        arguments: $arguments:ty;
    ) => {
        impl $crate::driver::Row for $row {
            fn try_value(
                &self,
                index: usize,
                kind: $crate::value::ValueKind,
            ) -> $crate::error::Result<$crate::value::Value> {
                try_value(self, index, kind)
            }

            fn is_null(&self, index: usize) -> $crate::error::Result<bool> {
                use sqlx::{Row as _, ValueRef as _};
                Ok(self
                    .try_get_raw(index)
                    .map_err($crate::driver::sqlx_backend::into_decode)?
                    .is_null())
            }
        }

        $(#[$driver_meta])*
        #[derive(Clone)]
        pub struct $driver {
            pool: $pool,
        }

        impl $driver {
            #[doc = $new_doc]
            #[must_use]
            pub const fn new(pool: $pool) -> Self {
                Self { pool }
            }

            #[doc = $pool_doc]
            #[must_use]
            pub const fn pool(&self) -> &$pool {
                &self.pool
            }
        }

        impl $crate::driver::Driver for $driver {
            fn dialect(&self) -> &'static dyn $crate::dialect::Dialect {
                &$dialect
            }

            fn fetch<'a>(
                &'a self,
                sql: String,
                binds: $crate::sql::BindBuffer,
                sink: &'a mut dyn $crate::driver::RowSink,
            ) -> futures::future::BoxFuture<'a, $crate::error::Result<()>> {
                Box::pin($crate::driver::sqlx_backend::fetch_into!(&self.pool, sql, binds, sink))
            }

            fn execute(
                &self,
                sql: String,
                binds: $crate::sql::BindBuffer,
            ) -> futures::future::BoxFuture<'_, $crate::error::Result<u64>> {
                Box::pin($crate::driver::sqlx_backend::execute_on!(&self.pool, sql, binds))
            }

            fn stream(
                &self,
                sql: String,
                binds: $crate::sql::BindBuffer,
            ) -> futures::stream::BoxStream<'_, $crate::error::Result<$crate::driver::BoxRow>>
            {
                Box::pin(async_stream::try_stream! {
                    let args = sqlx_args(binds)?;
                    let mut rows =
                        sqlx::query_with(sqlx::AssertSqlSafe(sql), args).fetch(&self.pool);
                    while let Some(row) = futures::TryStreamExt::try_next(&mut rows).await? {
                        yield Box::new(row) as $crate::driver::BoxRow;
                    }
                })
            }

            fn begin(
                &self,
            ) -> futures::future::BoxFuture<
                '_,
                $crate::error::Result<Box<dyn $crate::driver::TxConn>>,
            > {
                Box::pin(async move {
                    let transaction = self.pool.begin().await?;
                    Ok(Box::new($tx { transaction }) as Box<dyn $crate::driver::TxConn>)
                })
            }

            fn as_any(&self) -> &dyn core::any::Any {
                self
            }
        }

        $(#[$tx_meta])*
        struct $tx {
            transaction: sqlx::Transaction<'static, $db>,
        }

        impl $crate::driver::TxConn for $tx {
            fn fetch<'a>(
                &'a mut self,
                sql: String,
                binds: $crate::sql::BindBuffer,
                sink: &'a mut dyn $crate::driver::RowSink,
            ) -> futures::future::BoxFuture<'a, $crate::error::Result<()>> {
                Box::pin($crate::driver::sqlx_backend::fetch_into!(
                    &mut *self.transaction,
                    sql,
                    binds,
                    sink
                ))
            }

            fn execute(
                &mut self,
                sql: String,
                binds: $crate::sql::BindBuffer,
            ) -> futures::future::BoxFuture<'_, $crate::error::Result<u64>> {
                Box::pin($crate::driver::sqlx_backend::execute_on!(
                    &mut *self.transaction,
                    sql,
                    binds
                ))
            }

            fn commit(
                self: Box<Self>,
            ) -> futures::future::BoxFuture<'static, $crate::error::Result<()>> {
                Box::pin(async move {
                    self.transaction.commit().await?;
                    Ok(())
                })
            }

            fn rollback(
                self: Box<Self>,
            ) -> futures::future::BoxFuture<'static, $crate::error::Result<()>> {
                Box::pin(async move {
                    self.transaction.rollback().await?;
                    Ok(())
                })
            }
        }

        /// Convert backend-neutral binds into this backend's sqlx arguments.
        fn sqlx_args(binds: $crate::sql::BindBuffer) -> $crate::error::Result<$arguments> {
            let mut args = <$arguments>::default();
            for value in binds {
                bind_value(&mut args, value)?;
            }
            Ok(args)
        }

        /// Bind one value: typed nulls go to the backend's `bind_null`, everything
        /// else to its `bind_scalar`.
        fn bind_value(
            args: &mut $arguments,
            value: $crate::value::Value,
        ) -> $crate::error::Result<()> {
            match value {
                $crate::value::Value::Null(kind) => bind_null(args, kind),
                // The SQL writer unwraps `Cast` (or rejects it on backends without
                // `::` casts) before queueing the bind, so this is only a defensive
                // fallback — encode the inner payload.
                $crate::value::Value::Cast { inner, .. } => bind_value(args, *inner),
                other => bind_scalar(args, other),
            }
        }
    };
}

pub(super) use {
    add_null_scalar, add_scalar, execute_on, fetch_into, read_cell, read_scalar, sqlx_driver,
};
