//! Execution source: a pool-backed [`Driver`] or an in-progress transaction.
//! Query builders hold one of these so the same API runs on [`Db`](crate::Db) or
//! inside `db.transaction(..)`.
//!
//! Builders produce backend-neutral [`Value`](crate::value::Value)s; the driver
//! translates them to its native parameter type and yields rows as `dyn`
//! [`Row`](crate::driver::Row).

use crate::dialect::Dialect;
use crate::driver::{Driver, Row, RowSink, TxConn};
use crate::error::{Error, Result};
use crate::sql::BindBuffer;
use futures::lock::Mutex;
use std::sync::Arc;

/// Adapts a row-handling closure into a [`RowSink`], counting rows for logging.
struct FnSink<'f> {
    on_row: &'f mut (dyn FnMut(&dyn Row) -> Result<()> + Send),
    count: usize,
}

impl RowSink for FnSink<'_> {
    fn push(&mut self, row: &dyn Row) -> Result<()> {
        self.count += 1;
        (self.on_row)(row)
    }
}

/// A shared, in-progress transaction held as `Option` so finalization can take
/// it out regardless of how many query builders still reference the `Arc`.
///
/// Queries lock it sequentially (a transaction is inherently serial) — do not
/// drive two queries from the same transaction concurrently; the lock would
/// serialize them and a future awaiting the lock it already holds would hang.
pub(crate) type SharedTx = Arc<Mutex<Option<Box<dyn TxConn>>>>;

/// Error when a query runs after its transaction has been committed/rolled back.
const fn finished() -> Error {
    Error::Transaction("transaction already finished")
}

/// Where a query runs.
#[derive(Clone)]
pub(crate) enum Exec {
    /// Acquire a connection from the driver's pool per query.
    Pool(Arc<dyn Driver>),
    /// Run on the shared transaction's connection (dialect carried alongside,
    /// since the transaction handle is type-erased).
    Tx(&'static dyn Dialect, SharedTx),
}

// The transaction lock is intentionally held across the query's await: the query
// must run on the locked connection. Dropping it earlier would be incorrect.
#[allow(clippy::significant_drop_tightening)]
impl Exec {
    /// The SQL dialect of the underlying backend (for rendering placeholders,
    /// list membership, etc.).
    pub(crate) fn dialect(&self) -> &'static dyn Dialect {
        match self {
            Self::Pool(driver) => driver.dialect(),
            Self::Tx(dialect, _) => *dialect,
        }
    }
    /// Run a query, invoking `on_row` for each row as it arrives. Rows are decoded
    /// inline (the `&dyn Row` is borrowed, never boxed), so the collect path makes
    /// no per-row allocation.
    pub(crate) async fn for_each_row(
        &self,
        sql: String,
        binds: BindBuffer,
        mut on_row: impl FnMut(&dyn Row) -> Result<()> + Send,
    ) -> Result<()> {
        let started = log_sql(&sql);
        let mut sink = FnSink {
            on_row: &mut on_row,
            count: 0,
        };
        match self {
            Self::Pool(driver) => driver.fetch(sql, binds, &mut sink).await?,
            Self::Tx(_, shared) => {
                let mut guard = shared.lock().await;
                let transaction = guard.as_mut().ok_or_else(finished)?;
                transaction.fetch(sql, binds, &mut sink).await?;
            }
        }
        log_done(started, sink.count);
        Ok(())
    }

    /// Execute a statement, returning rows affected.
    pub(crate) async fn execute(&self, sql: String, binds: BindBuffer) -> Result<u64> {
        let started = log_sql(&sql);
        let affected = match self {
            Self::Pool(driver) => driver.execute(sql, binds).await?,
            Self::Tx(_, shared) => {
                let mut guard = shared.lock().await;
                let transaction = guard.as_mut().ok_or_else(finished)?;
                transaction.execute(sql, binds).await?
            }
        };
        log_done(started, usize::try_from(affected).unwrap_or(usize::MAX));
        Ok(affected)
    }
}

/// Log the SQL at `trace` (never bind values) and start the timer.
fn log_sql(sql: &str) -> std::time::Instant {
    tracing::trace!(target: "stakit_orm::query", sql = %sql);
    std::time::Instant::now()
}

/// Log elapsed time + affected/returned row count at `debug` (no values).
fn log_done(started: std::time::Instant, rows: usize) {
    tracing::debug!(
        target: "stakit_orm::query",
        rows,
        elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0,
        "query complete",
    );
}
