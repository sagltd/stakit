//! Execution source: a pool or an in-progress transaction. Query builders hold
//! one of these so the same API runs on `Db` or inside `db.transaction(..)`.

use crate::error::{Error, Result};
use futures::lock::Mutex;
use sqlx::postgres::{PgArguments, PgRow};
use sqlx::{PgPool, Postgres, Transaction};
use std::sync::Arc;

/// A shared, in-progress transaction held as `Option` so finalization can take
/// it out regardless of how many query builders still reference the `Arc`.
///
/// Queries lock it sequentially (a transaction is inherently serial) — do not
/// drive two queries from the same transaction concurrently; the lock would
/// serialize them and a future awaiting the lock it already holds would hang.
pub(crate) type SharedTx = Arc<Mutex<Option<Transaction<'static, Postgres>>>>;

/// Error when a query runs after its transaction has been committed/rolled back.
const fn finished() -> Error {
    Error::Transaction("transaction already finished")
}

/// Where a query runs.
#[derive(Clone)]
pub(crate) enum Exec {
    /// Acquire a connection from the pool per query.
    Pool(PgPool),
    /// Run on the shared transaction's connection.
    Tx(SharedTx),
}

// The transaction lock is intentionally held across the query's await: the query
// must run on the locked connection. Dropping it earlier would be incorrect.
#[allow(clippy::significant_drop_tightening)]
impl Exec {
    /// Fetch all rows.
    pub(crate) async fn fetch_all(&self, sql: String, args: PgArguments) -> Result<Vec<PgRow>> {
        let started = log_sql(&sql);
        let query = sqlx::query_with(sqlx::AssertSqlSafe(sql), args);
        let rows = match self {
            Self::Pool(pool) => query.fetch_all(pool).await?,
            Self::Tx(shared) => {
                let mut guard = shared.lock().await;
                let transaction = guard.as_mut().ok_or_else(finished)?;
                query.fetch_all(&mut **transaction).await?
            }
        };
        log_done(started, rows.len());
        Ok(rows)
    }

    /// Fetch at most one row.
    pub(crate) async fn fetch_optional(
        &self,
        sql: String,
        args: PgArguments,
    ) -> Result<Option<PgRow>> {
        let started = log_sql(&sql);
        let query = sqlx::query_with(sqlx::AssertSqlSafe(sql), args);
        let row = match self {
            Self::Pool(pool) => query.fetch_optional(pool).await?,
            Self::Tx(shared) => {
                let mut guard = shared.lock().await;
                let transaction = guard.as_mut().ok_or_else(finished)?;
                query.fetch_optional(&mut **transaction).await?
            }
        };
        log_done(started, usize::from(row.is_some()));
        Ok(row)
    }

    /// Fetch exactly one row.
    pub(crate) async fn fetch_one(&self, sql: String, args: PgArguments) -> Result<PgRow> {
        let started = log_sql(&sql);
        let query = sqlx::query_with(sqlx::AssertSqlSafe(sql), args);
        let row = match self {
            Self::Pool(pool) => query.fetch_one(pool).await?,
            Self::Tx(shared) => {
                let mut guard = shared.lock().await;
                let transaction = guard.as_mut().ok_or_else(finished)?;
                query.fetch_one(&mut **transaction).await?
            }
        };
        log_done(started, 1);
        Ok(row)
    }

    /// Execute a statement, returning rows affected.
    pub(crate) async fn execute(&self, sql: String, args: PgArguments) -> Result<u64> {
        let started = log_sql(&sql);
        let query = sqlx::query_with(sqlx::AssertSqlSafe(sql), args);
        let result = match self {
            Self::Pool(pool) => query.execute(pool).await?,
            Self::Tx(shared) => {
                let mut guard = shared.lock().await;
                let transaction = guard.as_mut().ok_or_else(finished)?;
                query.execute(&mut **transaction).await?
            }
        };
        let affected = result.rows_affected();
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
