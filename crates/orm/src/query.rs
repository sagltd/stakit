//! The `SELECT` builder and its terminals.
//!
//! State accumulates across chained methods; the SQL string + bind buffer are
//! assembled once at a terminal (`all`/`one`/...), never per step.

use crate::error::{Error, Result};
use crate::exec::Exec;
use crate::expr::{Order, Predicate};
use crate::projection::Projection;
use crate::schema::Table;
use crate::sql::{BindBuffer, SqlWriter};
use crate::value::Value;
use crate::vector::{Distance, DistanceSql};
use smallvec::SmallVec;

struct Join {
    keyword: &'static str,
    table: &'static str,
    on: Predicate,
}

/// A nearest-neighbour ordering: `ORDER BY distance(column, query)`.
struct NearestOrder {
    table: &'static str,
    name: &'static str,
    query: Vec<f32>,
    metric: Distance,
}

/// A `PostGIS` KNN ordering: `ORDER BY "table"."name" <-> $N::geometry`.
struct NearestGeo {
    table: &'static str,
    name: &'static str,
    geom: Value,
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
    nearest: Option<NearestOrder>,
    nearest_geo: Option<NearestGeo>,
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
            nearest: None,
            nearest_geo: None,
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

    /// Order by vector distance to `query` (nearest-neighbour search): appends
    /// `ORDER BY <distance>(column, query)` rendered for the active backend
    /// (`<->`/`<=>`/`<#>` on pgvector, `vector_distance_*` on Turso,
    /// `vec_distance_*` on `sqlite-vec`). Combine with `.limit(k)` for top-k. See
    /// [`crate::vector`].
    #[must_use]
    pub fn nearest<T, Ty>(
        mut self,
        column: crate::schema::Col<T, Ty>,
        query: &[f32],
        metric: Distance,
    ) -> Self {
        self.nearest = Some(NearestOrder {
            table: column.table,
            name: column.name,
            query: query.to_vec(),
            metric,
        });
        self
    }

    /// Order by `PostGIS` distance to `geom` (KNN nearest-neighbour search): appends
    /// `ORDER BY "table"."name" <-> $N::geometry`, which `PostGIS` answers from a
    /// `GiST` index. Combine with `.limit(k)` for top-k. See [`crate::geo`].
    #[must_use]
    pub fn nearest_geo<T, Ty>(
        mut self,
        column: crate::schema::Col<T, Ty>,
        geom: impl crate::geo::IntoGeo,
    ) -> Self {
        self.nearest_geo = Some(NearestGeo {
            table: column.table,
            name: column.name,
            geom: geom.into_geo_value(),
        });
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
    fn into_sql(self) -> Result<(P, String, BindBuffer)> {
        let dialect = self
            .exec
            .as_ref()
            .map_or_else(crate::dialect::default_dialect, Exec::dialect);
        let Self {
            projection,
            from_table,
            joins,
            filter,
            group,
            having,
            order,
            nearest,
            nearest_geo,
            limit,
            offset,
            exec: _,
        } = self;
        let mut writer = SqlWriter::with_dialect(dialect);
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
        let mut order_started = false;
        for (index, order) in order.iter().enumerate() {
            writer.push(if index == 0 { " order by " } else { ", " });
            order.write(&mut writer)?;
            order_started = true;
        }
        if let Some(nearest) = nearest {
            writer.push(if order_started { ", " } else { " order by " });
            match writer.vector_distance(nearest.metric) {
                DistanceSql::Operator(op) => {
                    writer.push_qualified(nearest.table, nearest.name)?;
                    writer.push(op);
                    writer.push_bind(Value::Vector(nearest.query));
                }
                DistanceSql::Function(function) => {
                    writer.push(function);
                    writer.push("(");
                    writer.push_qualified(nearest.table, nearest.name)?;
                    writer.push(", ");
                    writer.push_bind(Value::Vector(nearest.query));
                    writer.push(")");
                }
            }
            writer.push(" asc");
            order_started = true;
        }
        if let Some(geo) = nearest_geo {
            // `<->` is a PostGIS/pgvector operator — flag non-Postgres so the
            // terminal errors early instead of emitting SQL the DB would reject.
            if !writer.supports_spatial() {
                writer.mark_unsupported("PostGIS");
            }
            writer.push(if order_started { ", " } else { " order by " });
            // `PostGIS` KNN: `<col> <-> $N::geometry` (the cast is added by push_bind).
            writer.push_qualified(geo.table, geo.name)?;
            writer.push(" <-> ");
            writer.push_bind(geo.geom);
            writer.push(" asc");
        }
        if let Some(limit) = limit {
            writer.push(" limit ");
            writer.push_bind(Value::I64(limit));
        }
        if let Some(offset) = offset {
            writer.push(" offset ");
            writer.push_bind(Value::I64(offset));
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
    pub async fn all(self) -> Result<Vec<P::Output>>
    where
        P: Sync,
        P::Output: Send,
    {
        let exec = self.take_exec()?;
        let (projection, sql, arguments) = self.into_sql()?;
        let mut out = Vec::new();
        exec.for_each_row(sql, arguments, |row| {
            out.push(projection.decode(row, 0)?);
            Ok(())
        })
        .await?;
        Ok(out)
    }

    /// Run and return the first row, if any (adds `LIMIT 1`).
    ///
    /// # Errors
    /// Returns an error if the query fails or the row fails to decode.
    pub async fn one(mut self) -> Result<Option<P::Output>>
    where
        P: Sync,
        P::Output: Send,
    {
        self.limit = Some(1);
        let exec = self.take_exec()?;
        let (projection, sql, arguments) = self.into_sql()?;
        let mut out = None;
        exec.for_each_row(sql, arguments, |row| {
            if out.is_none() {
                out = Some(projection.decode(row, 0)?);
            }
            Ok(())
        })
        .await?;
        Ok(out)
    }

    /// Run and return the first row, erroring if absent.
    ///
    /// # Errors
    /// [`Error::NotFound`] if no row; query/decode errors otherwise.
    pub async fn one_or_err(self) -> Result<P::Output>
    where
        P: Sync,
        P::Output: Send,
    {
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
        self.nearest = None;
        self.nearest_geo = None;
        let exec = self.take_exec()?;
        let (_projection, inner, arguments) = self.into_sql()?;
        // Pre-size + push instead of `format!` to avoid a second full-length copy.
        let mut sql = String::with_capacity(inner.len() + 32);
        sql.push_str("select count(*) from (");
        sql.push_str(&inner);
        sql.push_str(") as __count");
        let mut value = 0_i64;
        exec.for_each_row(sql, arguments, |row| {
            value = crate::driver::decode_cell(row, 0)?;
            Ok(())
        })
        .await?;
        Ok(value)
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
        self.nearest = None;
        self.nearest_geo = None;
        let exec = self.take_exec()?;
        let (_projection, inner, arguments) = self.into_sql()?;
        let mut sql = String::with_capacity(inner.len() + 16);
        sql.push_str("select exists(");
        sql.push_str(&inner);
        sql.push(')');
        let mut value = false;
        exec.for_each_row(sql, arguments, |row| {
            value = crate::driver::decode_cell(row, 0)?;
            Ok(())
        })
        .await?;
        Ok(value)
    }

    /// Stream rows lazily (bounded client memory). Pool-only: streaming inside a
    /// transaction returns an error item (the transaction lock model is serial).
    /// The stream holds a pooled connection for its lifetime.
    pub fn stream(self) -> impl futures::Stream<Item = Result<P::Output>> {
        async_stream::try_stream! {
            let exec = self.exec.clone().ok_or(Error::NotFound)?;
            let (projection, sql, arguments) = self.into_sql()?;
            let driver = match exec {
                Exec::Pool(driver) => driver,
                Exec::Tx(..) => {
                    Err(Error::Transaction("stream is not supported inside a transaction"))?
                }
            };
            let mut rows = driver.stream(sql, arguments);
            while let Some(row) = futures::TryStreamExt::try_next(&mut rows).await? {
                yield projection.decode(row.as_ref(), 0)?;
            }
        }
    }

    /// Run and require exactly one row.
    ///
    /// # Errors
    /// [`Error::NotFound`] if no row, [`Error::TooManyRows`] if more than one.
    pub async fn exact_one(mut self) -> Result<P::Output>
    where
        P: Sync,
        P::Output: Send,
    {
        self.limit = Some(2);
        let exec = self.take_exec()?;
        let (projection, sql, arguments) = self.into_sql()?;
        let mut rows: SmallVec<[P::Output; 2]> = SmallVec::new();
        exec.for_each_row(sql, arguments, |row| {
            rows.push(projection.decode(row, 0)?);
            Ok(())
        })
        .await?;
        if rows.len() > 1 {
            return Err(Error::TooManyRows);
        }
        rows.into_iter().next().ok_or(Error::NotFound)
    }
}
