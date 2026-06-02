//! The [`Db`] handle and [`Tx`] transaction handle: thin wrappers that hand out
//! query builders bound to a pool or an in-progress transaction.

use crate::driver::Driver;
#[cfg(feature = "mysql")]
use crate::driver::MySqlDriver;
#[cfg(feature = "postgres")]
use crate::driver::PostgresDriver;
#[cfg(feature = "sqlite")]
use crate::driver::SqliteDriver;
#[cfg(feature = "turso")]
use crate::driver::TursoDriver;
use crate::error::{Error, Result};
use crate::exec::{Exec, SharedTx};
use crate::insert::{Insert, Insertable};
use crate::mutation::{Delete, Update};
use crate::projection::Projection;
use crate::query::Select;
use crate::raw::Raw;
use crate::schema::Table;
use futures::lock::Mutex;
#[cfg(feature = "postgres")]
use sqlx::PgPool;
#[cfg(feature = "postgres")]
use sqlx::postgres::PgPoolOptions;
use std::future::Future;
use std::sync::Arc;
#[cfg(feature = "postgres")]
use std::time::Duration;

/// Production connection-pool configuration (Postgres). `Debug` redacts the URL
/// (it carries credentials).
#[cfg(feature = "postgres")]
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

#[cfg(feature = "postgres")]
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

#[cfg(feature = "postgres")]
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

/// One ordered, versioned migration: a unique `version` and the SQL `statements`
/// to apply (each statement runs individually, so it works on every backend). Use
/// with [`Db::migrate`].
#[derive(Debug, Clone, Copy)]
pub struct Migration<'a> {
    /// Unique, ordered identifier (e.g. `"0001_init"`); recorded once applied.
    pub version: &'a str,
    /// SQL statements to run, in order.
    pub statements: &'a [&'a str],
}

/// A database handle. Cheap to clone (the driver is reference-counted) and `Send`
/// + `Sync`; share it across tasks.
#[derive(Clone)]
pub struct Db {
    driver: Arc<dyn Driver>,
}

impl Db {
    /// Wrap an existing sqlx Postgres pool.
    #[cfg(feature = "postgres")]
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self::from_driver(Arc::new(PostgresDriver::new(pool)))
    }

    /// Build a handle over any [`Driver`] (the backend-agnostic constructor).
    #[must_use]
    pub fn from_driver(driver: Arc<dyn Driver>) -> Self {
        Self { driver }
    }

    /// Connect to a Postgres `url` with sqlx defaults.
    ///
    /// # Errors
    /// Returns an error if the connection cannot be established.
    #[cfg(feature = "postgres")]
    pub async fn connect(url: &str) -> Result<Self> {
        Ok(Self::new(PgPool::connect(url).await?))
    }

    /// Wrap an existing sqlx `SQLite` pool.
    #[cfg(feature = "sqlite")]
    #[must_use]
    pub fn sqlite(pool: sqlx::SqlitePool) -> Self {
        Self::from_driver(Arc::new(SqliteDriver::new(pool)))
    }

    /// Connect to a `SQLite` `url` (e.g. `sqlite::memory:` or `sqlite://file.db`).
    ///
    /// # Errors
    /// Returns an error if the connection cannot be established.
    #[cfg(feature = "sqlite")]
    pub async fn connect_sqlite(url: &str) -> Result<Self> {
        Ok(Self::sqlite(sqlx::SqlitePool::connect(url).await?))
    }

    /// Wrap an existing sqlx `MySQL` pool.
    #[cfg(feature = "mysql")]
    #[must_use]
    pub fn mysql(pool: sqlx::MySqlPool) -> Self {
        Self::from_driver(Arc::new(MySqlDriver::new(pool)))
    }

    /// Connect to a `MySQL` `url`.
    ///
    /// # Errors
    /// Returns an error if the connection cannot be established.
    #[cfg(feature = "mysql")]
    pub async fn connect_mysql(url: &str) -> Result<Self> {
        Ok(Self::mysql(sqlx::MySqlPool::connect(url).await?))
    }

    /// Wrap an existing Turso / `libSQL` [`Connection`](libsql::Connection).
    #[cfg(feature = "turso")]
    #[must_use]
    pub fn turso(connection: libsql::Connection) -> Self {
        Self::from_driver(Arc::new(TursoDriver::new(connection)))
    }

    /// Open a local Turso / `libSQL` database at `path` (use `:memory:` for an
    /// in-memory database).
    ///
    /// # Errors
    /// Returns an error if the database cannot be opened or connected.
    #[cfg(feature = "turso")]
    pub async fn connect_turso_local(path: &str) -> Result<Self> {
        let database = libsql::Builder::new_local(path)
            .build()
            .await
            .map_err(Error::Turso)?;
        let connection = database.connect().map_err(Error::Turso)?;
        Ok(Self::turso(connection))
    }

    /// Connect to a remote Turso / `libSQL` database (`url` + auth `token`).
    ///
    /// # Errors
    /// Returns an error if the database cannot be opened or connected.
    #[cfg(feature = "turso")]
    pub async fn connect_turso_remote(url: &str, token: &str) -> Result<Self> {
        let database = libsql::Builder::new_remote(url.to_owned(), token.to_owned())
            .build()
            .await
            .map_err(Error::Turso)?;
        let connection = database.connect().map_err(Error::Turso)?;
        Ok(Self::turso(connection))
    }

    /// Connect using an explicit [`DbConfig`] (pool sizing, timeouts).
    ///
    /// # Errors
    /// Returns an error if the connection cannot be established.
    #[cfg(feature = "postgres")]
    pub async fn connect_with(config: &DbConfig) -> Result<Self> {
        let options = PgPoolOptions::new()
            .max_connections(config.max_connections)
            .min_connections(config.min_connections)
            .acquire_timeout(config.acquire_timeout)
            .idle_timeout(config.idle_timeout)
            .max_lifetime(config.max_lifetime);
        Ok(Self::new(options.connect(&config.url).await?))
    }

    /// Borrow the underlying sqlx Postgres pool, if this handle is Postgres-backed
    /// (the unaudited raw escape hatch, e.g. for the migrator). Returns `None` for
    /// other backends.
    #[cfg(feature = "postgres")]
    #[must_use]
    pub fn pool(&self) -> Option<&PgPool> {
        self.driver
            .as_any()
            .downcast_ref::<PostgresDriver>()
            .map(PostgresDriver::pool)
    }

    fn exec(&self) -> Exec {
        Exec::Pool(Arc::clone(&self.driver))
    }

    /// Apply pending [`Migration`]s in order, on **any** backend (Postgres, `SQLite`,
    /// `MySQL`, Turso) — migrations work out-of-box because they run through the
    /// [`Driver`], not a backend-specific migrator.
    ///
    /// Applied versions are tracked in a `_stakit_migrations` table; each pending
    /// migration runs in a transaction (its statements then the version record) so a
    /// failure rolls back and is safely retried. (On `MySQL`, DDL implicitly commits,
    /// so a multi-statement migration that fails mid-way is not atomic — the standard
    /// `MySQL` caveat.) Returns the number of migrations applied.
    ///
    /// # Errors
    /// Returns an error if a migration statement fails or tracking can't be read.
    pub async fn migrate(&self, migrations: &[Migration<'_>]) -> Result<u64> {
        let exec = self.exec();
        exec.execute(
            "create table if not exists _stakit_migrations (version varchar(255) primary key)"
                .to_owned(),
            crate::sql::BindBuffer::new(),
        )
        .await?;

        let mut applied = std::collections::HashSet::new();
        exec.for_each_row(
            "select version from _stakit_migrations".to_owned(),
            crate::sql::BindBuffer::new(),
            |row| {
                applied.insert(crate::driver::decode_cell::<String>(row, 0)?);
                Ok(())
            },
        )
        .await?;

        let dialect = self.driver.dialect();
        let placeholder = if dialect.numbered_placeholders() {
            format!("{}1", dialect.placeholder_prefix())
        } else {
            dialect.placeholder_prefix().to_string()
        };
        let insert_sql = format!("insert into _stakit_migrations (version) values ({placeholder})");

        let mut count = 0;
        for migration in migrations {
            if applied.contains(migration.version) {
                continue;
            }
            let insert_sql = insert_sql.clone();
            self.transaction(|tx| async move {
                for statement in migration.statements {
                    tx.raw(*statement).exec().await?;
                }
                tx.raw(insert_sql)
                    .bind(migration.version.to_owned())
                    .exec()
                    .await?;
                Ok(())
            })
            .await?;
            count += 1;
        }
        Ok(count)
    }

    /// Start a `SELECT` for `projection`, bound to this pool.
    pub fn select<P: Projection>(&self, projection: P) -> Select<P> {
        Select::with_exec(projection, self.exec())
    }

    /// Start a whole-row `SELECT * FROM T` (Drizzle-style `find`): the output type
    /// is inferred as `T`, so no `T::all()` / `.from::<T>()` boilerplate and no
    /// `let rows: Vec<T>` annotation. Chain `.filter()`/`.order_by()`/`.limit()`
    /// then a terminal (`.all()` → `Vec<T>`, `.one()` → `Option<T>`).
    #[must_use]
    pub fn find<T: Table>(&self) -> Select<crate::projection::All<T>> {
        self.select(crate::projection::All::<T>::new()).from::<T>()
    }

    /// Fetch by primary key: `SELECT * FROM T WHERE <pk> = $1`, output inferred as
    /// `T`. Finish with `.one()` → `Option<T>`. Tables without a primary key match
    /// nothing (the filter renders as always-false).
    #[must_use]
    pub fn get<T: Table>(&self, pk: T::Pk) -> Select<crate::projection::All<T>>
    where
        T::Pk: crate::value::ToValue,
    {
        self.find::<T>().filter(pk_filter::<T>(pk))
    }

    /// Load a **has-many** relation for a set of parents in **one** batched query
    /// (no N+1): fetches all children whose `child_fk` is in the parents' keys, then
    /// groups them, returning each parent paired with its children. Typed end to end
    /// — `child_fk: Col<C, K>` forces the foreign-key type to match the parent key —
    /// and backend-agnostic (the `IN` membership works on every driver).
    ///
    /// `parent_key` reads the key from a parent (usually its PK); `child_key` reads
    /// the same key from a loaded child (its FK), used to group.
    ///
    /// # Errors
    /// Returns an error if the child query fails.
    pub async fn load_has_many<P, C, K>(
        &self,
        parents: Vec<P>,
        child_fk: crate::schema::Col<C, K>,
        parent_key: impl Fn(&P) -> K,
        child_key: impl Fn(&C) -> K,
    ) -> Result<Vec<(P, Vec<C>)>>
    where
        C: Table + Send,
        K: crate::value::ToValue + crate::value::FromValue + Clone + Eq + core::hash::Hash,
    {
        if parents.is_empty() {
            return Ok(Vec::new());
        }
        let keys: Vec<K> = parents.iter().map(&parent_key).collect();
        let children: Vec<C> = self
            .find::<C>()
            .filter(crate::expr::any_of(child_fk, &keys))
            .all()
            .await?;
        let mut grouped: std::collections::HashMap<K, Vec<C>> = std::collections::HashMap::new();
        for child in children {
            grouped.entry(child_key(&child)).or_default().push(child);
        }
        Ok(parents
            .into_iter()
            .map(|parent| {
                let children = grouped.remove(&parent_key(&parent)).unwrap_or_default();
                (parent, children)
            })
            .collect())
    }

    /// Load a **belongs-to** relation for a set of children in one batched query:
    /// fetches the referenced parents by primary key, then pairs each child with its
    /// parent (`None` if the FK doesn't resolve). Typed and backend-agnostic.
    ///
    /// `child_key` reads the FK from a child; `parent_pk`/`parent_key` are the parent
    /// PK column and its accessor.
    ///
    /// # Errors
    /// Returns an error if the parent query fails.
    pub async fn load_belongs_to<C, P, K>(
        &self,
        children: Vec<C>,
        child_key: impl Fn(&C) -> K,
        parent_pk: crate::schema::Col<P, K>,
        parent_key: impl Fn(&P) -> K,
    ) -> Result<Vec<(C, Option<P>)>>
    where
        P: Table + Clone + Send,
        K: crate::value::ToValue + crate::value::FromValue + Clone + Eq + core::hash::Hash,
    {
        if children.is_empty() {
            return Ok(Vec::new());
        }
        let keys: Vec<K> = children.iter().map(&child_key).collect();
        let parents: Vec<P> = self
            .find::<P>()
            .filter(crate::expr::any_of(parent_pk, &keys))
            .all()
            .await?;
        let by_key: std::collections::HashMap<K, P> = parents
            .into_iter()
            .map(|parent| (parent_key(&parent), parent))
            .collect();
        // A parent may be shared by several children, so clone on attach.
        Ok(children
            .into_iter()
            .map(|child| {
                let parent = by_key.get(&child_key(&child)).cloned();
                (child, parent)
            })
            .collect())
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
        let transaction = self.driver.begin().await?;
        let shared: SharedTx = Arc::new(Mutex::new(Some(transaction)));
        let handle = Tx {
            exec: Exec::Tx(self.driver.dialect(), Arc::clone(&shared)),
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

    /// Start a whole-row `SELECT * FROM T` on this transaction (see [`Db::find`]).
    #[must_use]
    pub fn find<T: Table>(&self) -> Select<crate::projection::All<T>> {
        self.select(crate::projection::All::<T>::new()).from::<T>()
    }

    /// Fetch by primary key on this transaction (see [`Db::get`]).
    #[must_use]
    pub fn get<T: Table>(&self, pk: T::Pk) -> Select<crate::projection::All<T>>
    where
        T::Pk: crate::value::ToValue,
    {
        self.find::<T>().filter(pk_filter::<T>(pk))
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

/// Build the `WHERE <pk> = $1` predicate for `T::get`. Falls back to the
/// always-false `1 = 0` for a primary-key-less table (so `get` matches nothing
/// rather than every row).
fn pk_filter<T: Table>(pk: T::Pk) -> crate::expr::Predicate
where
    T::Pk: crate::value::ToValue,
{
    use crate::value::ToValue;
    T::COLUMNS.iter().find(|column| column.is_pk).map_or_else(
        || crate::expr::raw_pred("1 = 0"),
        |column| crate::expr::Predicate::eq_value(T::TABLE, column.name, pk.to_value()),
    )
}
