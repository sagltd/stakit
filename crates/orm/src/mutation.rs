//! `UPDATE` and `DELETE` builders. Values bind as `$N`; identifiers are quoted.

use crate::error::{Error, Result};
use crate::exec::Exec;
use crate::expr::{IntoExpr, Operand, Predicate};
use crate::schema::{Col, Table};
use crate::sql::BindBuffer;
use crate::sql::SqlWriter;

/// An `UPDATE` statement under construction.
pub struct Update<T> {
    exec: Option<Exec>,
    sets: Vec<(&'static str, Operand)>,
    filter: Option<Predicate>,
    marker: core::marker::PhantomData<fn() -> T>,
}

impl<T: Table> Update<T> {
    pub(crate) fn with_exec(exec: Exec) -> Self {
        Self {
            exec: Some(exec),
            sets: Vec::new(),
            filter: None,
            marker: core::marker::PhantomData,
        }
    }

    /// Builder not bound to an executor (for SQL inspection / unit tests).
    #[must_use]
    pub fn new() -> Self {
        Self {
            exec: None,
            sets: Vec::new(),
            filter: None,
            marker: core::marker::PhantomData,
        }
    }

    /// Set `column = value`.
    #[must_use]
    pub fn set<Ty, V>(mut self, column: Col<T, Ty>, value: V) -> Self
    where
        V: IntoExpr<Ty>,
    {
        self.sets.push((column.name, value.into_operand()));
        self
    }

    /// Set the `WHERE` predicate.
    #[must_use]
    pub fn filter(mut self, predicate: Predicate) -> Self {
        self.filter = Some(predicate);
        self
    }

    fn into_sql(self) -> Result<(String, BindBuffer)> {
        if self.sets.is_empty() {
            return Err(Error::NotFound);
        }
        let dialect = self
            .exec
            .as_ref()
            .map_or_else(crate::dialect::default_dialect, Exec::dialect);
        let mut writer = SqlWriter::with_dialect(dialect);
        writer.push("update ");
        writer.push_ident(T::TABLE)?;
        writer.push(" set ");
        for (index, (column, value)) in self.sets.into_iter().enumerate() {
            if index > 0 {
                writer.push(", ");
            }
            writer.push_ident(column)?;
            writer.push(" = ");
            value.write_into(&mut writer)?;
        }
        if let Some(filter) = self.filter {
            writer.push(" where ");
            filter.write(&mut writer)?;
        }
        crate::render::finish(writer)
    }

    /// Render the SQL (for inspection / unit tests).
    ///
    /// # Errors
    /// Returns an error if an identifier is invalid or no `set` was given.
    pub fn to_sql(self) -> Result<String> {
        Ok(self.into_sql()?.0)
    }

    /// Execute and return the number of rows affected.
    ///
    /// # Errors
    /// Returns an error if the statement fails.
    pub async fn exec(self) -> Result<u64> {
        let exec = self.exec.clone().ok_or(Error::NotFound)?;
        let (sql, arguments) = self.into_sql()?;
        exec.execute(sql, arguments).await
    }
}

impl<T: Table> Default for Update<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// A `DELETE` statement under construction.
pub struct Delete<T> {
    exec: Option<Exec>,
    filter: Option<Predicate>,
    marker: core::marker::PhantomData<fn() -> T>,
}

impl<T: Table> Delete<T> {
    pub(crate) fn with_exec(exec: Exec) -> Self {
        Self {
            exec: Some(exec),
            filter: None,
            marker: core::marker::PhantomData,
        }
    }

    /// Builder not bound to an executor (for SQL inspection / unit tests).
    #[must_use]
    pub fn new() -> Self {
        Self {
            exec: None,
            filter: None,
            marker: core::marker::PhantomData,
        }
    }

    /// Set the `WHERE` predicate.
    #[must_use]
    pub fn filter(mut self, predicate: Predicate) -> Self {
        self.filter = Some(predicate);
        self
    }

    fn into_sql(self) -> Result<(String, BindBuffer)> {
        let dialect = self
            .exec
            .as_ref()
            .map_or_else(crate::dialect::default_dialect, Exec::dialect);
        let mut writer = SqlWriter::with_dialect(dialect);
        writer.push("delete from ");
        writer.push_ident(T::TABLE)?;
        if let Some(filter) = self.filter {
            writer.push(" where ");
            filter.write(&mut writer)?;
        }
        crate::render::finish(writer)
    }

    /// Render the SQL (for inspection / unit tests).
    ///
    /// # Errors
    /// Returns an error if an identifier is invalid.
    pub fn to_sql(self) -> Result<String> {
        Ok(self.into_sql()?.0)
    }

    /// Execute and return the number of rows affected.
    ///
    /// # Errors
    /// Returns an error if the statement fails.
    pub async fn exec(self) -> Result<u64> {
        let exec = self.exec.clone().ok_or(Error::NotFound)?;
        let (sql, arguments) = self.into_sql()?;
        exec.execute(sql, arguments).await
    }
}

impl<T: Table> Default for Delete<T> {
    fn default() -> Self {
        Self::new()
    }
}
