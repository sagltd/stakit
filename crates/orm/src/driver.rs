//! Backend-neutral execution and result rows.
//!
//! The core never names a concrete sqlx `Database` type. A [`Driver`] renders the
//! right [`Dialect`](crate::dialect::Dialect), binds [`Value`]s, runs statements,
//! and yields rows as `dyn` [`Row`]; a projection reads cells back as [`Value`]s
//! and converts them with [`FromValue`]. This is what lets one ORM run on
//! Postgres, `SQLite`, `MySQL`, and Turso.

use crate::dialect::Dialect;
use crate::error::Result;
use crate::sql::BindBuffer;
use crate::value::{FromValue, Value, ValueKind};
use futures::future::BoxFuture;
use futures::stream::BoxStream;

#[cfg(feature = "mysql")]
mod mysql;
#[cfg(feature = "postgres")]
mod postgres;
#[cfg(feature = "sqlite")]
mod sqlite;
#[cfg(feature = "turso")]
mod turso;

#[cfg(feature = "mysql")]
pub use mysql::MySqlDriver;
#[cfg(feature = "postgres")]
pub use postgres::PostgresDriver;
#[cfg(feature = "sqlite")]
pub use sqlite::SqliteDriver;
#[cfg(feature = "turso")]
pub use turso::TursoDriver;

/// A type-erased, owned result row. Used only by the lazy [`stream`](Driver::stream)
/// path, which must yield owned items; the collect path decodes inline through a
/// [`RowSink`] and never allocates per row.
pub type BoxRow = Box<dyn Row + Send>;

/// Receives fetched rows in order — the zero-per-row-alloc collect path.
///
/// The driver calls [`push`](RowSink::push) for each row as it arrives, passing a
/// borrowed `&dyn Row` that lives only for the call — so the implementation
/// decodes immediately and the driver never heap-allocates a row.
pub trait RowSink: Send {
    /// Accept (and typically decode) one row.
    ///
    /// # Errors
    /// Returns an error if the row fails to decode; the driver stops the fetch.
    fn push(&mut self, row: &dyn Row) -> Result<()>;
}

/// Decode the cell at `index` as `T` through the [`Row`]/[`FromValue`] path. The
/// single leaf-decode helper used by projections and generated `from_row_at`.
///
/// # Errors
/// Returns an error if the cell cannot be read or converted to `T`.
pub fn decode_cell<T: FromValue>(row: &dyn Row, index: usize) -> Result<T> {
    T::from_value(row.try_value(index, T::KIND)?)
}

/// A backend-neutral result row: read a cell as a typed [`Value`].
pub trait Row {
    /// Read the cell at column ordinal `index`, decoding it as `kind`. A SQL
    /// `NULL` yields [`Value::Null`].
    ///
    /// # Errors
    /// Returns [`Error::Decode`](crate::error::Error::Decode) if the cell cannot
    /// be read as the requested kind.
    fn try_value(&self, index: usize, kind: ValueKind) -> Result<Value>;

    /// Whether the cell at `index` is SQL `NULL`.
    ///
    /// # Errors
    /// Returns [`Error::Decode`](crate::error::Error::Decode) if the ordinal is
    /// out of range.
    fn is_null(&self, index: usize) -> Result<bool>;
}

/// A database backend: dialect + statement execution over neutral binds and rows.
///
/// Cloning a [`Db`](crate::Db) clones an `Arc<dyn Driver>`, so a driver is shared,
/// not duplicated. Methods return boxed futures so the trait stays object-safe.
pub trait Driver: Send + Sync {
    /// The SQL dialect this backend renders.
    fn dialect(&self) -> &'static dyn Dialect;

    /// Run a query, pushing each row into `sink` as it arrives (no per-row
    /// allocation). The `sink` borrow lasts the whole fetch.
    fn fetch<'a>(
        &'a self,
        sql: String,
        binds: BindBuffer,
        sink: &'a mut dyn RowSink,
    ) -> BoxFuture<'a, Result<()>>;

    /// Run a statement and return the number of affected rows.
    fn execute(&self, sql: String, binds: BindBuffer) -> BoxFuture<'_, Result<u64>>;

    /// Stream rows lazily (bounded client memory), holding a connection for the
    /// stream's lifetime.
    fn stream(&self, sql: String, binds: BindBuffer) -> BoxStream<'_, Result<BoxRow>>;

    /// Begin a transaction, returning a serial connection handle.
    fn begin(&self) -> BoxFuture<'_, Result<Box<dyn TxConn>>>;

    /// Escape hatch for backend-specific access (e.g. the raw sqlx pool used by
    /// the migrator). Returns `None` unless downcast to the concrete driver.
    fn as_any(&self) -> &dyn core::any::Any;
}

/// An in-progress transaction's connection. A transaction is inherently serial:
/// issue queries sequentially. Finalized by consuming `self` via
/// [`commit`](TxConn::commit) or [`rollback`](TxConn::rollback).
pub trait TxConn: Send {
    /// Run a query on the transaction, pushing each row into `sink`.
    fn fetch<'a>(
        &'a mut self,
        sql: String,
        binds: BindBuffer,
        sink: &'a mut dyn RowSink,
    ) -> BoxFuture<'a, Result<()>>;

    /// Run a statement on the transaction, returning affected rows.
    fn execute(&mut self, sql: String, binds: BindBuffer) -> BoxFuture<'_, Result<u64>>;

    /// Commit the transaction.
    fn commit(self: Box<Self>) -> BoxFuture<'static, Result<()>>;

    /// Roll the transaction back.
    fn rollback(self: Box<Self>) -> BoxFuture<'static, Result<()>>;
}
