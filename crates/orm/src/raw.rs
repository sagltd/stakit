//! Typed raw-SQL escape hatch (§9). The SQL text is the caller's responsibility
//! (an explicit opt-out of the safe builder); values still bind as backend
//! placeholders.
//!
//! Results decode into a `#[derive(Table)]` type **positionally**, in
//! [`Table::COLUMNS`](crate::schema::Table) order — so `select *` (or an explicit
//! column list in that order) maps onto the struct.

use crate::error::Result;
use crate::exec::Exec;
use crate::schema::Table;
use crate::sql::BindBuffer;
use crate::value::ToValue;

/// A raw SQL query with bound parameters, decoding into any [`Table`] type.
pub struct Raw {
    exec: Exec,
    sql: String,
    binds: BindBuffer,
}

impl Raw {
    pub(crate) fn new(exec: Exec, sql: impl Into<String>) -> Self {
        Self {
            exec,
            sql: sql.into(),
            binds: BindBuffer::new(),
        }
    }

    /// Bind the next positional parameter (`$1`, `$2`, …).
    #[must_use]
    pub fn bind<T: ToValue>(mut self, value: T) -> Self {
        self.binds.push(value.to_value());
        self
    }

    fn parts(self) -> (Exec, String, BindBuffer) {
        (self.exec, self.sql, self.binds)
    }

    /// Fetch all rows, decoded positionally into `T`.
    ///
    /// # Errors
    /// Returns an error if the query fails or a row fails to decode.
    pub async fn all<T: Table>(self) -> Result<Vec<T>> {
        let (exec, sql, arguments) = self.parts();
        let mut out = Vec::new();
        exec.for_each_row(sql, arguments, |row| {
            out.push(T::from_row_at(row, 0)?);
            Ok(())
        })
        .await?;
        Ok(out)
    }

    /// Fetch at most one row, decoded positionally into `T`.
    ///
    /// # Errors
    /// Returns an error if the query fails or the row fails to decode.
    pub async fn one<T: Table>(self) -> Result<Option<T>> {
        let (exec, sql, arguments) = self.parts();
        let mut out = None;
        exec.for_each_row(sql, arguments, |row| {
            if out.is_none() {
                out = Some(T::from_row_at(row, 0)?);
            }
            Ok(())
        })
        .await?;
        Ok(out)
    }

    /// Execute a statement, returning the number of rows affected.
    ///
    /// # Errors
    /// Returns an error if the statement fails.
    pub async fn exec(self) -> Result<u64> {
        let (exec, sql, arguments) = self.parts();
        exec.execute(sql, arguments).await
    }
}
