//! Typed raw-SQL escape hatch (§9). The SQL text is the caller's responsibility
//! (an explicit opt-out of the safe builder); values still bind as `$N`.

use crate::error::{Error, Result};
use crate::exec::Exec;
use crate::sql::Bind;
use sqlx::FromRow;
use sqlx::postgres::PgArguments;

/// A raw SQL query with bound parameters, decoding into any [`FromRow`] type.
pub struct Raw {
    exec: Exec,
    sql: String,
    binds: Vec<Box<dyn Bind>>,
}

impl Raw {
    pub(crate) fn new(exec: Exec, sql: impl Into<String>) -> Self {
        Self {
            exec,
            sql: sql.into(),
            binds: Vec::new(),
        }
    }

    /// Bind the next positional parameter (`$1`, `$2`, …).
    #[must_use]
    pub fn bind<T>(mut self, value: T) -> Self
    where
        T: for<'q> sqlx::Encode<'q, sqlx::Postgres> + sqlx::Type<sqlx::Postgres> + Send + 'static,
    {
        self.binds.push(Box::new(value) as Box<dyn Bind>);
        self
    }

    fn parts(self) -> Result<(Exec, String, PgArguments)> {
        let mut arguments = PgArguments::default();
        for bind in self.binds {
            bind.add(&mut arguments).map_err(Error::Encode)?;
        }
        Ok((self.exec, self.sql, arguments))
    }

    /// Fetch all rows, decoded into `T`.
    ///
    /// # Errors
    /// Returns an error if the query fails or a row fails to decode.
    pub async fn all<T>(self) -> Result<Vec<T>>
    where
        T: for<'r> FromRow<'r, sqlx::postgres::PgRow>,
    {
        let (exec, sql, arguments) = self.parts()?;
        let rows = exec.fetch_all(sql, arguments).await?;
        rows.iter()
            .map(|row| T::from_row(row).map_err(Error::from))
            .collect()
    }

    /// Fetch at most one row, decoded into `T`.
    ///
    /// # Errors
    /// Returns an error if the query fails or the row fails to decode.
    pub async fn one<T>(self) -> Result<Option<T>>
    where
        T: for<'r> FromRow<'r, sqlx::postgres::PgRow>,
    {
        let (exec, sql, arguments) = self.parts()?;
        let row = exec.fetch_optional(sql, arguments).await?;
        row.map(|row| T::from_row(&row).map_err(Error::from))
            .transpose()
    }

    /// Execute a statement, returning the number of rows affected.
    ///
    /// # Errors
    /// Returns an error if the statement fails.
    pub async fn exec(self) -> Result<u64> {
        let (exec, sql, arguments) = self.parts()?;
        exec.execute(sql, arguments).await
    }
}
