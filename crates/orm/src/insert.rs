//! `INSERT` builder.
//!
//! A row type implements [`Insertable`] (generated as a `…New` companion by
//! `#[derive(Table)]`): required columns are plain fields, defaulted columns are
//! `Option` and, when `None` for every row in the batch, are omitted so the
//! database default fires.
//!
//! Caveat for batches: a defaulted column included because *some* row supplies it
//! is bound as `NULL` for the rows that left it `None` (a single multi-row
//! `VALUES` statement shares one column list). An empty batch is a no-op
//! (`exec` → 0, returning `all` → empty, `one` → `NotFound`).

use crate::error::{Error, Result};
use crate::exec::Exec;
use crate::projection::Projection;
use crate::sql::SqlWriter;
use smallvec::SmallVec;

/// A callback that writes the `RETURNING` projection columns.
type ReturningWriter<'a> = &'a dyn Fn(&mut SqlWriter) -> Result<()>;

/// Per-optional-column presence flags (inline up to 16 optional columns).
pub type OptionalPresent = SmallVec<[bool; 16]>;

/// A row insertable into a table. Implemented by the generated `…New` type.
pub trait Insertable {
    /// Target table name.
    const TABLE: &'static str;
    /// Required (always-present) column names, in order.
    const REQUIRED: &'static [&'static str];
    /// Optional (defaulted) column names, in order.
    const OPTIONAL: &'static [&'static str];
    /// Per-optional-column presence (`true` = this row supplies a value).
    fn optional_present(&self) -> OptionalPresent;
    /// Push this row's bind values, comma-separated, for the required columns
    /// followed by the optional columns flagged in `optional_included`.
    fn bind_values(self, optional_included: &[bool], writer: &mut SqlWriter);
}

/// How a single column is updated when a conflict fires.
#[derive(Clone, Copy)]
enum UpdateMode {
    /// `col = excluded.col` — overwrite with the incoming value.
    Excluded,
    /// `col = coalesce(excluded.col, <table>.col)` — take the incoming value, but
    /// keep the stored one when the incoming value is `NULL`.
    CoalesceExisting,
}

/// One column's update rule in a `DO UPDATE` / `ON DUPLICATE KEY UPDATE` clause.
#[derive(Clone, Copy)]
struct ColumnUpdate {
    column: &'static str,
    mode: UpdateMode,
}

/// What to do when an insert conflicts with the target key.
enum ConflictAction {
    /// `DO NOTHING` — leave the existing row untouched.
    Nothing,
    /// `DO UPDATE SET <every non-target inserted column> = excluded.<col>`.
    UpdateAll,
    /// `DO UPDATE` setting every non-target inserted column *except* these to its
    /// `excluded` value (e.g. keep an immutable `created_at`).
    UpdateAllExcept(SmallVec<[&'static str; 4]>),
    /// `DO UPDATE SET` only the listed columns, each with its chosen [`UpdateMode`].
    UpdateColumns(SmallVec<[ColumnUpdate; 8]>),
}

/// A resolved `ON CONFLICT` specification: the target key columns plus the action.
struct Conflict {
    /// The conflict-target columns (the unique/primary-key constraint). May be more
    /// than one for a composite key. Ignored by `MySQL` (which keys on any unique
    /// index via `ON DUPLICATE KEY UPDATE`).
    targets: SmallVec<[&'static str; 2]>,
    action: ConflictAction,
}

/// An `INSERT` under construction.
pub struct Insert<N> {
    exec: Option<Exec>,
    rows: Vec<N>,
    conflict: Option<Conflict>,
}

impl<N: Insertable> Insert<N> {
    pub(crate) const fn with_exec(exec: Exec, rows: Vec<N>) -> Self {
        Self {
            exec: Some(exec),
            rows,
            conflict: None,
        }
    }

    /// Builder not bound to an executor (for SQL inspection / unit tests).
    #[must_use]
    pub const fn new(rows: Vec<N>) -> Self {
        Self {
            exec: None,
            rows,
            conflict: None,
        }
    }

    /// `ON CONFLICT (column) DO NOTHING`.
    #[must_use]
    pub fn on_conflict_do_nothing<T, Ty>(mut self, column: crate::schema::Col<T, Ty>) -> Self {
        self.conflict = Some(Conflict {
            targets: smallvec::smallvec![column.name],
            action: ConflictAction::Nothing,
        });
        self
    }

    /// `ON CONFLICT (column) DO UPDATE` — sets every other inserted column to its
    /// `excluded` value (upsert).
    #[must_use]
    pub fn on_conflict_do_update<T, Ty>(mut self, column: crate::schema::Col<T, Ty>) -> Self {
        self.conflict = Some(Conflict {
            targets: smallvec::smallvec![column.name],
            action: ConflictAction::UpdateAll,
        });
        self
    }

    /// Begin an upsert on a conflict `key` — a single column (`Device::id`) or a
    /// tuple of columns for a composite key (`(Device::user_id, Device::device_id)`).
    ///
    /// Returns an [`Upsert`] builder: chain [`set`](Upsert::set) /
    /// [`set_coalesce`](Upsert::set_coalesce) to pick exactly which columns to
    /// refresh on conflict (every unlisted column is left untouched), or
    /// [`do_nothing`](Upsert::do_nothing) / [`do_update_all`](Upsert::do_update_all).
    ///
    /// The `key` must be backed by a unique or primary-key constraint (named in the
    /// `ON CONFLICT (...)` target on Postgres/`SQLite`/Turso; matched implicitly by
    /// `MySQL`'s `ON DUPLICATE KEY UPDATE`).
    pub fn on_conflict<K: ConflictKey>(self, key: K) -> Upsert<N> {
        Upsert {
            insert: self,
            targets: key.columns(),
            updates: SmallVec::new(),
        }
    }

    /// Which optional columns appear in any row (union); all-`None` optionals are
    /// omitted so the DB default applies. Inline up to 16 optional columns.
    fn optional_included(rows: &[N]) -> OptionalPresent {
        let mut included: OptionalPresent = smallvec::smallvec![false; N::OPTIONAL.len()];
        for row in rows {
            for (slot, present) in included.iter_mut().zip(row.optional_present()) {
                *slot = *slot || present;
            }
        }
        included
    }

    fn column_names(included: &[bool]) -> SmallVec<[&'static str; 16]> {
        let mut names: SmallVec<[&'static str; 16]> = SmallVec::from_slice(N::REQUIRED);
        for (name, present) in N::OPTIONAL.iter().zip(included) {
            if *present {
                names.push(name);
            }
        }
        names
    }

    fn write_head(writer: &mut SqlWriter, columns: &[&'static str]) -> Result<()> {
        writer.push("insert into ");
        writer.push_ident(N::TABLE)?;
        writer.push(" (");
        for (index, name) in columns.iter().enumerate() {
            if index > 0 {
                writer.push(", ");
            }
            writer.push_ident(name)?;
        }
        writer.push(") values ");
        Ok(())
    }

    fn render(self, returning: Option<ReturningWriter<'_>>) -> Result<(Option<Exec>, SqlWriter)> {
        let Self {
            exec,
            rows,
            conflict,
        } = self;
        // Fast path: tables with no defaulted columns skip the per-row
        // optional-presence scan entirely (the column list is exactly `REQUIRED`).
        let included = if N::OPTIONAL.is_empty() {
            OptionalPresent::new()
        } else {
            Self::optional_included(&rows)
        };
        let columns = Self::column_names(&included);
        let dialect = exec
            .as_ref()
            .map_or_else(crate::dialect::default_dialect, crate::exec::Exec::dialect);
        let mut writer = SqlWriter::with_dialect(dialect);
        Self::write_head(&mut writer, &columns)?;
        for (index, row) in rows.into_iter().enumerate() {
            if index > 0 {
                writer.push(", ");
            }
            writer.push("(");
            row.bind_values(&included, &mut writer);
            writer.push(")");
        }
        if let Some(conflict) = &conflict {
            write_conflict(&mut writer, conflict, N::TABLE, &columns)?;
        }
        if let Some(write_returning) = returning {
            writer.push(" returning ");
            write_returning(&mut writer)?;
        }
        Ok((exec, writer))
    }

    fn build(self, returning: Option<ReturningWriter<'_>>) -> Result<(Exec, SqlWriter)> {
        let (exec, writer) = self.render(returning)?;
        Ok((exec.ok_or(Error::NotFound)?, writer))
    }

    /// Error early if this builder's backend does not support `RETURNING` (`MySQL`),
    /// rather than emitting invalid SQL that only fails at the database.
    fn ensure_returning_supported(&self) -> Result<()> {
        if let Some(exec) = &self.exec {
            if !exec.dialect().supports_returning() {
                return Err(Error::Unsupported("RETURNING"));
            }
        }
        Ok(())
    }

    /// Render the SQL (for inspection / unit tests).
    ///
    /// # Errors
    /// Returns an error if an identifier is invalid.
    pub fn to_sql(self) -> Result<String> {
        if self.rows.is_empty() {
            return Ok(String::new());
        }
        let (_exec, writer) = self.render(None)?;
        Ok(writer.sql().to_owned())
    }

    /// Execute the insert, returning the number of rows inserted.
    ///
    /// # Errors
    /// Returns an error if the statement fails.
    pub async fn exec(self) -> Result<u64> {
        if self.rows.is_empty() {
            return Ok(0);
        }
        let (exec, writer) = self.build(None)?;
        let (sql, arguments) = crate::render::finish(writer)?;
        exec.execute(sql, arguments).await
    }

    /// Add a `RETURNING <projection>` clause.
    #[must_use]
    pub const fn returning<P: Projection>(self, projection: P) -> InsertReturning<N, P> {
        InsertReturning {
            insert: self,
            projection,
        }
    }
}

/// An `INSERT … RETURNING` under construction.
pub struct InsertReturning<N, P> {
    insert: Insert<N>,
    projection: P,
}

impl<N: Insertable, P: Projection> InsertReturning<N, P> {
    /// Execute and decode all returned rows.
    ///
    /// # Errors
    /// Returns an error if the statement fails or a row fails to decode.
    pub async fn all(self) -> Result<Vec<P::Output>>
    where
        P: Sync,
        P::Output: Send,
    {
        if self.insert.rows.is_empty() {
            return Ok(Vec::new());
        }
        self.insert.ensure_returning_supported()?;
        let projection_columns = projection_writer(&self.projection);
        let (exec, writer) = self.insert.build(Some(&projection_columns))?;
        let (sql, arguments) = crate::render::finish(writer)?;
        let mut out = Vec::new();
        exec.for_each_row(sql, arguments, |row| {
            out.push(self.projection.decode(row, 0)?);
            Ok(())
        })
        .await?;
        Ok(out)
    }

    /// Execute and decode a single returned row (a plain single-row insert always
    /// produces exactly one).
    ///
    /// # Errors
    /// [`Error::NotFound`] if no row was returned; decode/statement errors otherwise.
    pub async fn one(self) -> Result<P::Output>
    where
        P: Sync,
        P::Output: Send,
    {
        if self.insert.rows.is_empty() {
            return Err(Error::NotFound);
        }
        self.insert.ensure_returning_supported()?;
        let projection_columns = projection_writer(&self.projection);
        let (exec, writer) = self.insert.build(Some(&projection_columns))?;
        let (sql, arguments) = crate::render::finish(writer)?;
        let mut out = None;
        exec.for_each_row(sql, arguments, |row| {
            if out.is_none() {
                out = Some(self.projection.decode(row, 0)?);
            }
            Ok(())
        })
        .await?;
        out.ok_or(Error::NotFound)
    }
}

fn projection_writer<P: Projection>(projection: &P) -> impl Fn(&mut SqlWriter) -> Result<()> + '_ {
    move |writer| projection.write_columns(writer)
}

/// A conflict-target key for [`Insert::on_conflict`]: a single [`Col`](crate::schema::Col)
/// or a tuple of columns (2–4) for a composite key.
pub trait ConflictKey {
    /// The target column names, in order.
    #[must_use]
    fn columns(self) -> SmallVec<[&'static str; 2]>;
}

impl<T, Ty> ConflictKey for crate::schema::Col<T, Ty> {
    fn columns(self) -> SmallVec<[&'static str; 2]> {
        smallvec::smallvec![self.name]
    }
}

impl<T1, Y1, T2, Y2> ConflictKey for (crate::schema::Col<T1, Y1>, crate::schema::Col<T2, Y2>) {
    fn columns(self) -> SmallVec<[&'static str; 2]> {
        smallvec::smallvec![self.0.name, self.1.name]
    }
}

impl<T1, Y1, T2, Y2, T3, Y3> ConflictKey
    for (
        crate::schema::Col<T1, Y1>,
        crate::schema::Col<T2, Y2>,
        crate::schema::Col<T3, Y3>,
    )
{
    fn columns(self) -> SmallVec<[&'static str; 2]> {
        smallvec::smallvec![self.0.name, self.1.name, self.2.name]
    }
}

impl<T1, Y1, T2, Y2, T3, Y3, T4, Y4> ConflictKey
    for (
        crate::schema::Col<T1, Y1>,
        crate::schema::Col<T2, Y2>,
        crate::schema::Col<T3, Y3>,
        crate::schema::Col<T4, Y4>,
    )
{
    fn columns(self) -> SmallVec<[&'static str; 2]> {
        smallvec::smallvec![self.0.name, self.1.name, self.2.name, self.3.name]
    }
}

/// An upsert under construction: `INSERT … ON CONFLICT (<key>) DO …`.
///
/// Created by [`Insert::on_conflict`]. Choose which columns to refresh on conflict
/// with [`set`](Self::set) (overwrite) and [`set_coalesce`](Self::set_coalesce)
/// (overwrite, but keep the stored value when the incoming one is `NULL`); every
/// column you do not list is left untouched. Shortcuts: [`do_nothing`](Self::do_nothing)
/// and [`do_update_all`](Self::do_update_all).
///
/// Terminals mirror [`Insert`]: [`exec`](Self::exec), [`returning`](Self::returning),
/// [`to_sql`](Self::to_sql).
///
/// ```no_run
/// # use stakit_orm::prelude::*;
/// # #[derive(Table)] #[table(name = "devices")]
/// # struct Device {
/// #   #[column(pk)] id: i64,
/// #   user_id: i64,
/// #   #[column(unique)] device_id: String,
/// #   platform: String,
/// #   #[column(nullable)] location: Option<String>,
/// # }
/// # async fn f(db: stakit_orm::Db, row: DeviceNew) -> stakit_orm::Result<()> {
/// db.insert(row)
///     .on_conflict((Device::user_id, Device::device_id)) // composite key
///     .set(Device::platform)                             // platform = excluded.platform
///     .set_coalesce(Device::location)  // location = coalesce(excluded.location, devices.location)
///     .exec()
///     .await?;
/// # Ok(()) }
/// ```
#[must_use]
pub struct Upsert<N> {
    insert: Insert<N>,
    targets: SmallVec<[&'static str; 2]>,
    updates: SmallVec<[ColumnUpdate; 8]>,
}

impl<N: Insertable> Upsert<N> {
    /// On conflict, set `column = excluded.column` (overwrite with the incoming
    /// value).
    pub fn set<T, Ty>(mut self, column: crate::schema::Col<T, Ty>) -> Self {
        self.updates.push(ColumnUpdate {
            column: column.name,
            mode: UpdateMode::Excluded,
        });
        self
    }

    /// On conflict, set `column = coalesce(excluded.column, <table>.column)`: take
    /// the incoming value, but **keep the stored one when the incoming value is
    /// `NULL`**. Use for best-effort fields (e.g. an async-resolved location) that a
    /// later write must not erase.
    pub fn set_coalesce<T, Ty>(mut self, column: crate::schema::Col<T, Ty>) -> Self {
        self.updates.push(ColumnUpdate {
            column: column.name,
            mode: UpdateMode::CoalesceExisting,
        });
        self
    }

    /// `ON CONFLICT (<key>) DO NOTHING` — leave the existing row untouched. Discards
    /// any `set`/`set_coalesce` columns chosen so far.
    pub fn do_nothing(mut self) -> Insert<N> {
        self.insert.conflict = Some(Conflict {
            targets: self.targets,
            action: ConflictAction::Nothing,
        });
        self.insert
    }

    /// `DO UPDATE` setting *every* non-key inserted column to its `excluded` value
    /// (the overwrite-everything upsert — no need to list each column). Discards any
    /// explicit `set` columns chosen so far.
    pub fn do_update_all(mut self) -> Insert<N> {
        self.insert.conflict = Some(Conflict {
            targets: self.targets,
            action: ConflictAction::UpdateAll,
        });
        self.insert
    }

    /// Like [`do_update_all`](Self::do_update_all), but leave the given column(s)
    /// untouched on conflict — e.g. `do_update_all_except(Row::created_at)` or a tuple
    /// `do_update_all_except((Row::created_at, Row::id))`. Discards explicit `set`
    /// columns chosen so far.
    pub fn do_update_all_except<K: ConflictKey>(mut self, columns: K) -> Insert<N> {
        self.insert.conflict = Some(Conflict {
            targets: self.targets,
            action: ConflictAction::UpdateAllExcept(columns.columns().into_iter().collect()),
        });
        self.insert
    }

    /// Finalize the explicit-column upsert into the underlying [`Insert`]. With no
    /// `set`/`set_coalesce` columns chosen, falls back to `DO NOTHING` (an empty
    /// `SET` would be invalid SQL).
    fn into_insert(self) -> Insert<N> {
        let Self {
            mut insert,
            targets,
            updates,
        } = self;
        let action = if updates.is_empty() {
            ConflictAction::Nothing
        } else {
            ConflictAction::UpdateColumns(updates)
        };
        insert.conflict = Some(Conflict { targets, action });
        insert
    }

    /// Execute the upsert, returning the number of rows inserted or updated.
    ///
    /// # Errors
    /// Returns an error if the statement fails.
    pub async fn exec(self) -> Result<u64> {
        self.into_insert().exec().await
    }

    /// Add a `RETURNING <projection>` clause (Postgres / `SQLite` / Turso).
    pub fn returning<P: Projection>(self, projection: P) -> InsertReturning<N, P> {
        self.into_insert().returning(projection)
    }

    /// Render the SQL (for inspection / unit tests).
    ///
    /// # Errors
    /// Returns an error if an identifier is invalid.
    pub fn to_sql(self) -> Result<String> {
        self.into_insert().to_sql()
    }
}

fn write_conflict(
    writer: &mut SqlWriter,
    conflict: &Conflict,
    table: &str,
    columns: &[&'static str],
) -> Result<()> {
    // Resolve the action into a concrete column-update list. `UpdateAll` expands
    // to every non-target inserted column overwritten with its `excluded` value; an
    // empty list (or `Nothing`) means no `SET` clause.
    let resolved: SmallVec<[ColumnUpdate; 16]> = match &conflict.action {
        ConflictAction::Nothing => SmallVec::new(),
        ConflictAction::UpdateAll => columns
            .iter()
            .copied()
            .filter(|column| !conflict.targets.contains(column))
            .map(|column| ColumnUpdate {
                column,
                mode: UpdateMode::Excluded,
            })
            .collect(),
        ConflictAction::UpdateAllExcept(skip) => columns
            .iter()
            .copied()
            .filter(|column| !conflict.targets.contains(column) && !skip.contains(column))
            .map(|column| ColumnUpdate {
                column,
                mode: UpdateMode::Excluded,
            })
            .collect(),
        ConflictAction::UpdateColumns(updates) => updates.iter().copied().collect(),
    };
    let do_nothing = resolved.is_empty();

    // MySQL has no `ON CONFLICT`; it uses `ON DUPLICATE KEY UPDATE` keyed implicitly
    // on any unique/primary index. The incoming value is `values(col)`; a bare `col`
    // refers to the stored row.
    if writer.upsert_on_duplicate_key() {
        writer.push(" on duplicate key update ");
        if do_nothing {
            // MySQL lacks `DO NOTHING`; self-assign a key column as a harmless no-op.
            if let Some(column) = conflict
                .targets
                .first()
                .or_else(|| columns.first())
                .copied()
            {
                writer.push_ident(column)?;
                writer.push(" = ");
                writer.push_ident(column)?;
            }
            return Ok(());
        }
        for (index, update) in resolved.iter().enumerate() {
            if index > 0 {
                writer.push(", ");
            }
            writer.push_ident(update.column)?;
            writer.push(" = ");
            match update.mode {
                UpdateMode::Excluded => {
                    writer.push("values(");
                    writer.push_ident(update.column)?;
                    writer.push(")");
                }
                UpdateMode::CoalesceExisting => {
                    writer.push("coalesce(values(");
                    writer.push_ident(update.column)?;
                    writer.push("), ");
                    writer.push_ident(update.column)?;
                    writer.push(")");
                }
            }
        }
        return Ok(());
    }

    // Standard `ON CONFLICT (<targets>) DO …` (Postgres / SQLite / Turso).
    writer.push(" on conflict (");
    for (index, target) in conflict.targets.iter().enumerate() {
        if index > 0 {
            writer.push(", ");
        }
        writer.push_ident(target)?;
    }
    writer.push(")");
    // An empty `SET` would be invalid SQL — fall back to `DO NOTHING`.
    if do_nothing {
        writer.push(" do nothing");
        return Ok(());
    }
    writer.push(" do update set ");
    for (index, update) in resolved.iter().enumerate() {
        if index > 0 {
            writer.push(", ");
        }
        writer.push_ident(update.column)?;
        writer.push(" = ");
        match update.mode {
            UpdateMode::Excluded => {
                writer.push("excluded.");
                writer.push_ident(update.column)?;
            }
            UpdateMode::CoalesceExisting => {
                writer.push("coalesce(excluded.");
                writer.push_ident(update.column)?;
                writer.push(", ");
                writer.push_qualified(table, update.column)?;
                writer.push(")");
            }
        }
    }
    Ok(())
}

/// Helper for generated code: convert a field into a backend-neutral [`Value`].
#[doc(hidden)]
#[must_use]
pub fn boxed_bind<T: crate::value::ToValue>(value: T) -> crate::value::Value {
    value.to_value()
}

#[cfg(test)]
mod tests {
    use super::{ColumnUpdate, Conflict, ConflictAction, UpdateMode, write_conflict};
    use crate::dialect::{Dialect, MySqlDialect, PostgresDialect};
    use crate::sql::SqlWriter;
    use smallvec::SmallVec;

    const TABLE: &str = "devices";

    fn target(names: &[&'static str]) -> SmallVec<[&'static str; 2]> {
        names.iter().copied().collect()
    }

    fn render(
        dialect: &'static dyn Dialect,
        conflict: &Conflict,
        columns: &[&'static str],
    ) -> String {
        let mut writer = SqlWriter::with_dialect(dialect);
        write_conflict(&mut writer, conflict, TABLE, columns).unwrap();
        let (sql, _) = writer.into_parts();
        sql
    }

    fn do_update(updates: &[(&'static str, UpdateMode)]) -> ConflictAction {
        ConflictAction::UpdateColumns(
            updates
                .iter()
                .map(|&(column, mode)| ColumnUpdate { column, mode })
                .collect(),
        )
    }

    // ---- standard ON CONFLICT (Postgres / SQLite / Turso) ----

    #[test]
    fn pg_composite_key_selective_update_with_coalesce() {
        let conflict = Conflict {
            targets: target(&["user_id", "device_id"]),
            action: do_update(&[
                ("platform", UpdateMode::Excluded),
                ("last_seen", UpdateMode::Excluded),
                ("location", UpdateMode::CoalesceExisting),
            ]),
        };
        let sql = render(
            &PostgresDialect,
            &conflict,
            &["user_id", "device_id", "platform", "last_seen", "location"],
        );
        assert_eq!(
            sql,
            r#" on conflict ("user_id", "device_id") do update set "platform" = excluded."platform", "last_seen" = excluded."last_seen", "location" = coalesce(excluded."location", "devices"."location")"#
        );
    }

    #[test]
    fn pg_do_update_all_skips_target_columns() {
        let conflict = Conflict {
            targets: target(&["email"]),
            action: ConflictAction::UpdateAll,
        };
        let sql = render(&PostgresDialect, &conflict, &["email", "name", "id"]);
        assert_eq!(
            sql,
            r#" on conflict ("email") do update set "name" = excluded."name", "id" = excluded."id""#
        );
    }

    #[test]
    fn pg_do_nothing_renders_target_list() {
        let conflict = Conflict {
            targets: target(&["user_id", "device_id"]),
            action: ConflictAction::Nothing,
        };
        let sql = render(&PostgresDialect, &conflict, &["user_id", "device_id"]);
        assert_eq!(sql, r#" on conflict ("user_id", "device_id") do nothing"#);
    }

    #[test]
    fn pg_do_update_all_with_only_target_falls_back_to_do_nothing() {
        let conflict = Conflict {
            targets: target(&["email"]),
            action: ConflictAction::UpdateAll,
        };
        let sql = render(&PostgresDialect, &conflict, &["email"]);
        assert_eq!(sql, r#" on conflict ("email") do nothing"#);
    }

    #[test]
    fn pg_do_update_all_with_composite_target_skips_all_key_columns() {
        let conflict = Conflict {
            targets: target(&["user_id", "device_id"]),
            action: ConflictAction::UpdateAll,
        };
        let sql = render(
            &PostgresDialect,
            &conflict,
            &["user_id", "device_id", "platform", "last_seen"],
        );
        assert_eq!(
            sql,
            r#" on conflict ("user_id", "device_id") do update set "platform" = excluded."platform", "last_seen" = excluded."last_seen""#
        );
    }

    #[test]
    fn pg_do_update_all_except_skips_target_and_listed() {
        let conflict = Conflict {
            targets: target(&["user_id", "device_id"]),
            action: ConflictAction::UpdateAllExcept(std::iter::once("created_at").collect()),
        };
        let sql = render(
            &PostgresDialect,
            &conflict,
            &["user_id", "device_id", "platform", "created_at"],
        );
        assert_eq!(
            sql,
            r#" on conflict ("user_id", "device_id") do update set "platform" = excluded."platform""#
        );
    }

    // ---- MySQL ON DUPLICATE KEY UPDATE ----

    #[test]
    fn mysql_selective_update_with_coalesce_uses_values() {
        let conflict = Conflict {
            targets: target(&["user_id", "device_id"]),
            action: do_update(&[
                ("platform", UpdateMode::Excluded),
                ("location", UpdateMode::CoalesceExisting),
            ]),
        };
        let sql = render(
            &MySqlDialect,
            &conflict,
            &["user_id", "device_id", "platform", "location"],
        );
        assert_eq!(
            sql,
            " on duplicate key update `platform` = values(`platform`), `location` = coalesce(values(`location`), `location`)"
        );
    }

    #[test]
    fn mysql_do_update_all_uses_values() {
        let conflict = Conflict {
            targets: target(&["email"]),
            action: ConflictAction::UpdateAll,
        };
        let sql = render(&MySqlDialect, &conflict, &["email", "name", "id"]);
        assert_eq!(
            sql,
            " on duplicate key update `name` = values(`name`), `id` = values(`id`)"
        );
    }

    #[test]
    fn mysql_do_update_all_with_only_target_is_noop_self_assign() {
        let conflict = Conflict {
            targets: target(&["email"]),
            action: ConflictAction::UpdateAll,
        };
        let sql = render(&MySqlDialect, &conflict, &["email"]);
        assert_eq!(sql, " on duplicate key update `email` = `email`");
    }

    #[test]
    fn mysql_do_nothing_self_assigns_first_target() {
        let conflict = Conflict {
            targets: target(&["user_id", "device_id"]),
            action: ConflictAction::Nothing,
        };
        let sql = render(&MySqlDialect, &conflict, &["user_id", "device_id", "name"]);
        assert_eq!(sql, " on duplicate key update `user_id` = `user_id`");
    }
}
