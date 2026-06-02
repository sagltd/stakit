//! The [`Db`] handle and [`Tx`] transaction handle: thin wrappers that hand out
//! query builders bound to a pool or an in-progress transaction.

use crate::error::{Error, Result};
use crate::exec::{Exec, SharedTx};
use crate::insert::{Insert, Insertable};
use crate::mutation::{Delete, Update};
use crate::projection::Projection;
use crate::query::Select;
use crate::raw::Raw;
use crate::schema::Table;
use futures::lock::Mutex;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

/// Production connection-pool configuration. `Debug` redacts the URL (it carries
/// credentials).
#[derive(Clone)]
pub struct DbConfig {
    url: String,
    /// Maximum pooled connections.
    pub max_connections: u32,
    /// Minimum idle connections to keep.
    pub min_connections: u32,
    /// Bound on how long acquiring a connection may block (no unbounded waits).
    pub acquire_timeout: Duration,
    /// Close a connection after this idle time.
    pub idle_timeout: Option<Duration>,
    /// Recycle a connection after this total lifetime.
    pub max_lifetime: Option<Duration>,
}

impl DbConfig {
    /// Config with production-sane defaults for `url`.
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            max_connections: 10,
            min_connections: 0,
            acquire_timeout: Duration::from_secs(30),
            idle_timeout: Some(Duration::from_secs(600)),
            max_lifetime: Some(Duration::from_secs(1800)),
        }
    }
}

impl core::fmt::Debug for DbConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DbConfig")
            .field("url", &"<redacted>")
            .field("max_connections", &self.max_connections)
            .field("min_connections", &self.min_connections)
            .field("acquire_timeout", &self.acquire_timeout)
            .field("idle_timeout", &self.idle_timeout)
            .field("max_lifetime", &self.max_lifetime)
            .finish()
    }
}

/// A database handle. Cheap to clone (the pool is reference-counted) and `Send`
/// + `Sync`; share it across tasks.
#[derive(Clone)]
pub struct Db {
    pool: PgPool,
}

impl Db {
    /// Wrap an existing sqlx pool.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Connect to `url` with sqlx defaults.
    ///
    /// # Errors
    /// Returns an error if the connection cannot be established.
    pub async fn connect(url: &str) -> Result<Self> {
        Ok(Self {
            pool: PgPool::connect(url).await?,
        })
    }

    /// Connect using an explicit [`DbConfig`] (pool sizing, timeouts).
    ///
    /// # Errors
    /// Returns an error if the connection cannot be established.
    pub async fn connect_with(config: &DbConfig) -> Result<Self> {
        let options = PgPoolOptions::new()
            .max_connections(config.max_connections)
            .min_connections(config.min_connections)
            .acquire_timeout(config.acquire_timeout)
            .idle_timeout(config.idle_timeout)
            .max_lifetime(config.max_lifetime);
        Ok(Self {
            pool: options.connect(&config.url).await?,
        })
    }

    /// Borrow the underlying sqlx pool (the unaudited raw escape hatch).
    #[must_use]
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }

    fn exec(&self) -> Exec {
        Exec::Pool(self.pool.clone())
    }

    /// Start a `SELECT` for `projection`, bound to this pool.
    pub fn select<P: Projection>(&self, projection: P) -> Select<P> {
        Select::with_exec(projection, self.exec())
    }

    /// Insert a single row.
    pub fn insert<N: Insertable>(&self, row: N) -> Insert<N> {
        Insert::with_exec(self.exec(), vec![row])
    }

    /// Insert many rows in one statement.
    pub fn insert_many<N: Insertable>(&self, rows: Vec<N>) -> Insert<N> {
        Insert::with_exec(self.exec(), rows)
    }

    /// Start an `UPDATE` for table `T`, bound to this pool.
    #[must_use]
    pub fn update<T: Table>(&self) -> Update<T> {
        Update::with_exec(self.exec())
    }

    /// Start a `DELETE` for table `T`, bound to this pool.
    #[must_use]
    pub fn delete<T: Table>(&self) -> Delete<T> {
        Delete::with_exec(self.exec())
    }

    /// Start a raw SQL query (the explicit, unaudited escape hatch).
    #[must_use]
    pub fn raw(&self, sql: impl Into<String>) -> Raw {
        Raw::new(self.exec(), sql)
    }

    /// Run `work` inside a transaction. On `Ok` the transaction commits; on `Err`
    /// (or a returned error) it rolls back.
    ///
    /// Issue queries on the [`Tx`] handle **sequentially** (a transaction is
    /// serial); driving two queries from one `Tx` concurrently would serialize on
    /// an internal lock. A builder that outlives the closure simply observes a
    /// finished transaction and errors — it cannot corrupt commit/rollback.
    ///
    /// # Errors
    /// Propagates the closure's error (after rollback) or any begin/commit error.
    pub async fn transaction<F, Fut, R>(&self, work: F) -> Result<R>
    where
        F: FnOnce(Tx) -> Fut,
        Fut: Future<Output = Result<R>>,
    {
        let transaction = self.pool.begin().await?;
        let shared: SharedTx = Arc::new(Mutex::new(Some(transaction)));
        let handle = Tx {
            exec: Exec::Tx(Arc::clone(&shared)),
        };
        let outcome = work(handle).await;

        // Take the transaction out of the shared cell. This works regardless of
        // how many builder clones still hold the `Arc` (a stray builder would
        // observe a finished transaction and error), so it cannot turn a
        // successful closure into a phantom rollback.
        let taken = shared.lock().await.take();
        let Some(transaction) = taken else {
            return Err(Error::Transaction("transaction already finalized"));
        };
        match outcome {
            Ok(value) => {
                transaction.commit().await?;
                Ok(value)
            }
            Err(error) => {
                let _ = transaction.rollback().await;
                Err(error)
            }
        }
    }
}

/// A transaction handle, handing out builders that run on the transaction's
/// connection. Queries are issued sequentially (a transaction is serial).
pub struct Tx {
    exec: Exec,
}

impl Tx {
    /// Start a `SELECT` on this transaction.
    pub fn select<P: Projection>(&self, projection: P) -> Select<P> {
        Select::with_exec(projection, self.exec.clone())
    }

    /// Insert a single row on this transaction.
    pub fn insert<N: Insertable>(&self, row: N) -> Insert<N> {
        Insert::with_exec(self.exec.clone(), vec![row])
    }

    /// Insert many rows on this transaction.
    pub fn insert_many<N: Insertable>(&self, rows: Vec<N>) -> Insert<N> {
        Insert::with_exec(self.exec.clone(), rows)
    }

    /// Start an `UPDATE` on this transaction.
    #[must_use]
    pub fn update<T: Table>(&self) -> Update<T> {
        Update::with_exec(self.exec.clone())
    }

    /// Start a `DELETE` on this transaction.
    #[must_use]
    pub fn delete<T: Table>(&self) -> Delete<T> {
        Delete::with_exec(self.exec.clone())
    }

    /// Start a raw SQL query on this transaction.
    #[must_use]
    pub fn raw(&self, sql: impl Into<String>) -> Raw {
        Raw::new(self.exec.clone(), sql)
    }
}
