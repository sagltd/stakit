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

/// `ON CONFLICT` behavior.
#[derive(Clone, Copy)]
enum Conflict {
    /// `ON CONFLICT (target) DO NOTHING`.
    DoNothing(&'static str),
    /// `ON CONFLICT (target) DO UPDATE SET <other cols> = excluded.<col>`.
    DoUpdate(&'static str),
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
    pub const fn on_conflict_do_nothing<T, Ty>(
        mut self,
        column: crate::schema::Col<T, Ty>,
    ) -> Self {
        self.conflict = Some(Conflict::DoNothing(column.name));
        self
    }

    /// `ON CONFLICT (column) DO UPDATE` — sets every other inserted column to its
    /// `excluded` value (upsert).
    #[must_use]
    pub const fn on_conflict_do_update<T, Ty>(mut self, column: crate::schema::Col<T, Ty>) -> Self {
        self.conflict = Some(Conflict::DoUpdate(column.name));
        self
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
        let included = Self::optional_included(&rows);
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
        if let Some(conflict) = conflict {
            write_conflict(&mut writer, conflict, &columns)?;
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
        let (sql, arguments) = crate::render::finish(writer);
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
        let (sql, arguments) = crate::render::finish(writer);
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
        let (sql, arguments) = crate::render::finish(writer);
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

fn write_conflict(writer: &mut SqlWriter, conflict: Conflict, columns: &[&str]) -> Result<()> {
    match conflict {
        Conflict::DoNothing(target) => {
            writer.push(" on conflict (");
            writer.push_ident(target)?;
            writer.push(") do nothing");
        }
        Conflict::DoUpdate(target) => {
            let setters: SmallVec<[&str; 16]> =
                columns.iter().copied().filter(|&c| c != target).collect();
            writer.push(" on conflict (");
            writer.push_ident(target)?;
            // With no other columns to update, `DO UPDATE SET` would be invalid
            // SQL — fall back to DO NOTHING.
            if setters.is_empty() {
                writer.push(") do nothing");
                return Ok(());
            }
            writer.push(") do update set ");
            for (index, column) in setters.iter().enumerate() {
                if index > 0 {
                    writer.push(", ");
                }
                writer.push_ident(column)?;
                writer.push(" = excluded.");
                writer.push_ident(column)?;
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
