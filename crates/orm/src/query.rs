//! The `SELECT` builder and its terminals.
//!
//! State accumulates across chained methods; the SQL string + bind buffer are
//! assembled once at a terminal (`all`/`one`/...), never per step.

use crate::error::{Error, Result};
use crate::exec::Exec;
use crate::expr::{Order, Predicate};
use crate::projection::Projection;
use crate::schema::Table;
use crate::sql::{Bind, SqlWriter};
use smallvec::SmallVec;
use sqlx::Row;
use sqlx::postgres::PgArguments;

struct Join {
    keyword: &'static str,
    table: &'static str,
    on: Predicate,
}

/// A `SELECT` query under construction.
pub struct Select<P> {
    projection: P,
    exec: Option<Exec>,
    from_table: &'static str,
    joins: Vec<Join>,
    filter: Option<Predicate>,
    group: SmallVec<[(&'static str, &'static str); 4]>,
    having: Option<Predicate>,
    order: SmallVec<[Order; 4]>,
    limit: Option<i64>,
    offset: Option<i64>,
}

impl<P: Projection> Select<P> {
    /// Create a builder for `projection` not bound to a pool (for SQL inspection
    /// and unit tests).
    #[must_use]
    pub fn new(projection: P) -> Self {
        Self {
            projection,
            exec: None,
            from_table: "",
            joins: Vec::new(),
            filter: None,
            group: SmallVec::new(),
            having: None,
            order: SmallVec::new(),
            limit: None,
            offset: None,
        }
    }

    pub(crate) fn with_exec(projection: P, exec: Exec) -> Self {
        let mut select = Self::new(projection);
        select.exec = Some(exec);
        select
    }

    /// Set the `FROM` table.
    #[must_use]
    pub const fn from<T: Table>(mut self) -> Self {
        self.from_table = T::TABLE;
        self
    }

    /// Add an `INNER JOIN T ON <predicate>`.
    #[must_use]
    pub fn inner_join<T: Table>(mut self, on: Predicate) -> Self {
        self.joins.push(Join {
            keyword: "inner join",
            table: T::TABLE,
            on,
        });
        self
    }

    /// Add a `LEFT JOIN T ON <predicate>`.
    #[must_use]
    pub fn left_join<T: Table>(mut self, on: Predicate) -> Self {
        self.joins.push(Join {
            keyword: "left join",
            table: T::TABLE,
            on,
        });
        self
    }

    /// Add a `RIGHT JOIN T ON <predicate>`.
    #[must_use]
    pub fn right_join<T: Table>(mut self, on: Predicate) -> Self {
        self.joins.push(Join {
            keyword: "right join",
            table: T::TABLE,
            on,
        });
        self
    }

    /// Add a `GROUP BY` column.
    #[must_use]
    pub fn group_by<T, Ty>(mut self, column: crate::schema::Col<T, Ty>) -> Self {
        self.group.push((column.table, column.name));
        self
    }

    /// Set the `HAVING` predicate. NOTE: predicates currently compare grouped
    /// *columns* only; aggregate-valued `HAVING` (e.g. `count(..) > n`) is a
    /// follow-up — use a raw query for that today.
    #[must_use]
    pub fn having(mut self, predicate: Predicate) -> Self {
        self.having = Some(predicate);
        self
    }

    /// Set the `WHERE` predicate.
    #[must_use]
    pub fn filter(mut self, predicate: Predicate) -> Self {
        self.filter = Some(predicate);
        self
    }

    /// Append an `ORDER BY` term.
    #[must_use]
    pub fn order_by(mut self, order: Order) -> Self {
        self.order.push(order);
        self
    }

    /// Set `LIMIT` (bound as a parameter).
    #[must_use]
    pub const fn limit(mut self, limit: i64) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Set `OFFSET` (bound as a parameter).
    #[must_use]
    pub const fn offset(mut self, offset: i64) -> Self {
        self.offset = Some(offset);
        self
    }

    /// Consume the builder into `(projection, sql, arguments)`. Predicates are
    /// `FnOnce`, so building is consuming; the projection is handed back for
    /// row decoding.
    fn into_sql(self) -> Result<(P, String, PgArguments)> {
        let Self {
            projection,
            from_table,
            joins,
            filter,
            group,
            having,
            order,
            limit,
            offset,
            exec: _,
        } = self;
        let mut writer = SqlWriter::new();
        writer.push("select ");
        projection.write_columns(&mut writer)?;
        writer.push(" from ");
        writer.push_ident(from_table)?;
        for join in joins {
            writer.push(" ");
            writer.push(join.keyword);
            writer.push(" ");
            writer.push_ident(join.table)?;
            writer.push(" on ");
            join.on.write(&mut writer)?;
        }
        if let Some(filter) = filter {
            writer.push(" where ");
            filter.write(&mut writer)?;
        }
        for (index, (table, column)) in group.iter().enumerate() {
            writer.push(if index == 0 { " group by " } else { ", " });
            writer.push_qualified(table, column)?;
        }
        if let Some(having) = having {
            writer.push(" having ");
            having.write(&mut writer)?;
        }
        for (index, order) in order.iter().enumerate() {
            writer.push(if index == 0 { " order by " } else { ", " });
            order.write(&mut writer)?;
        }
        if let Some(limit) = limit {
            writer.push(" limit ");
            writer.push_bind(Box::new(limit) as Box<dyn Bind>);
        }
        if let Some(offset) = offset {
            writer.push(" offset ");
            writer.push_bind(Box::new(offset) as Box<dyn Bind>);
        }
        let (sql, arguments) = crate::render::finish(writer)?;
        Ok((projection, sql, arguments))
    }

    /// Render the SQL text (for inspection / unit tests), discarding binds.
    ///
    /// # Errors
    /// Returns an error if an identifier is invalid.
    pub fn to_sql(self) -> Result<String> {
        let (_projection, sql, _arguments) = self.into_sql()?;
        Ok(sql)
    }

    fn take_exec(&self) -> Result<Exec> {
        self.exec.clone().ok_or(Error::NotFound)
    }

    /// Run and collect all rows.
    ///
    /// # Errors
    /// Returns an error if the query fails or a row fails to decode.
    pub async fn all(self) -> Result<Vec<P::Output>> {
        let exec = self.take_exec()?;
        let (projection, sql, arguments) = self.into_sql()?;
        let rows = exec.fetch_all(sql, arguments).await?;
        rows.iter().map(|row| projection.decode(row, 0)).collect()
    }

    /// Run and return the first row, if any (adds `LIMIT 1`).
    ///
    /// # Errors
    /// Returns an error if the query fails or the row fails to decode.
    pub async fn one(mut self) -> Result<Option<P::Output>> {
        self.limit = Some(1);
        let exec = self.take_exec()?;
        let (projection, sql, arguments) = self.into_sql()?;
        let row = exec.fetch_optional(sql, arguments).await?;
        row.map(|row| projection.decode(&row, 0)).transpose()
    }

    /// Run and return the first row, erroring if absent.
    ///
    /// # Errors
    /// [`Error::NotFound`] if no row; query/decode errors otherwise.
    pub async fn one_or_err(self) -> Result<P::Output> {
        self.one().await?.ok_or(Error::NotFound)
    }

    /// Count matching rows (`select count(*) from (<this query>)`), ignoring the
    /// projection. `LIMIT`/`OFFSET`/`ORDER BY` are dropped so the count reflects
    /// the total matching rows, not a paged window.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub async fn count(mut self) -> Result<i64> {
        self.limit = None;
        self.offset = None;
        self.order.clear();
        let exec = self.take_exec()?;
        let (_projection, inner, arguments) = self.into_sql()?;
        let sql = format!("select count(*) from ({inner}) as __count");
        let row = exec.fetch_one(sql, arguments).await?;
        Ok(row.try_get(0)?)
    }

    /// Whether any row matches (`select exists(<this query>)`). `LIMIT`/`OFFSET`/
    /// `ORDER BY` are dropped (irrelevant to existence).
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub async fn exists(mut self) -> Result<bool> {
        self.limit = None;
        self.offset = None;
        self.order.clear();
        let exec = self.take_exec()?;
        let (_projection, inner, arguments) = self.into_sql()?;
        let sql = format!("select exists({inner})");
        let row = exec.fetch_one(sql, arguments).await?;
        Ok(row.try_get(0)?)
    }

    /// Stream rows lazily (bounded client memory). Pool-only: streaming inside a
    /// transaction returns an error item (the transaction lock model is serial).
    /// The stream holds a pooled connection for its lifetime.
    pub fn stream(self) -> impl futures::Stream<Item = Result<P::Output>> {
        async_stream::try_stream! {
            let exec = self.exec.clone().ok_or(Error::NotFound)?;
            let (projection, sql, arguments) = self.into_sql()?;
            let pool = match exec {
                Exec::Pool(pool) => pool,
                Exec::Tx(_) => {
                    Err(Error::Transaction("stream is not supported inside a transaction"))?
                }
            };
            let mut rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), arguments).fetch(&pool);
            while let Some(row) = futures::TryStreamExt::try_next(&mut rows).await? {
                yield projection.decode(&row, 0)?;
            }
        }
    }

    /// Run and require exactly one row.
    ///
    /// # Errors
    /// [`Error::NotFound`] if no row, [`Error::TooManyRows`] if more than one.
    pub async fn exact_one(mut self) -> Result<P::Output> {
        self.limit = Some(2);
        let exec = self.take_exec()?;
        let (projection, sql, arguments) = self.into_sql()?;
        let rows = exec.fetch_all(sql, arguments).await?;
        match rows.split_first() {
            None => Err(Error::NotFound),
            Some((row, [])) => projection.decode(row, 0),
            Some(_) => Err(Error::TooManyRows),
        }
    }
}
