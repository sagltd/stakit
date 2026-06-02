//! Typed errors.
//!
//! sqlx errors are mapped to Postgres-semantic variants via SQLSTATE, reading
//! only `code`/`constraint`/`column` — never `message`/`detail` (which can embed
//! values). The transparent fallback can still carry a raw pg message, so
//! `Display` must be logged server-side, not shown to clients.

use crate::ident::IdentError;
use sqlx::error::{BoxDynError, DatabaseError};

/// Result alias for stakit-orm operations.
pub type Result<T> = core::result::Result<T, Error>;

/// A database or query-building error.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No row where exactly one was required.
    #[error("not found")]
    NotFound,
    /// More than one row where exactly one was required.
    #[error("too many rows: expected one")]
    TooManyRows,
    /// Unique violation (SQLSTATE 23505).
    #[error("unique violation on {constraint}")]
    Unique {
        /// Constraint name from the database.
        constraint: String,
    },
    /// Foreign-key violation (SQLSTATE 23503).
    #[error("foreign key violation on {constraint}")]
    ForeignKey {
        /// Constraint name from the database.
        constraint: String,
    },
    /// Not-null violation (SQLSTATE 23502).
    #[error("not-null violation on {column}")]
    NotNull {
        /// Offending column name from the database.
        column: String,
    },
    /// Check violation (SQLSTATE 23514).
    #[error("check violation on {constraint}")]
    Check {
        /// Constraint name from the database.
        constraint: String,
    },
    /// An identifier could not be rendered safely.
    #[error("invalid identifier: {0}")]
    Ident(#[from] IdentError),
    /// A transaction was used incorrectly (e.g. the handle escaped the closure).
    #[error("transaction misuse: {0}")]
    Transaction(&'static str),
    /// A value failed to decode from a row.
    #[error(transparent)]
    Decode(BoxDynError),
    /// A bind value failed to encode for a parameter.
    #[error(transparent)]
    Encode(BoxDynError),
    /// Any other sqlx error (transparent — may carry a raw pg message).
    #[error(transparent)]
    Database(sqlx::Error),
}

impl Error {
    /// Whether this is a unique violation.
    #[must_use]
    pub const fn is_unique(&self) -> bool {
        matches!(self, Self::Unique { .. })
    }

    /// Whether this is a foreign-key violation.
    #[must_use]
    pub const fn is_foreign_key(&self) -> bool {
        matches!(self, Self::ForeignKey { .. })
    }

    /// Whether this is a not-found error.
    #[must_use]
    pub const fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound)
    }
}

impl From<sqlx::Error> for Error {
    fn from(error: sqlx::Error) -> Self {
        if matches!(error, sqlx::Error::RowNotFound) {
            return Self::NotFound;
        }
        let sqlx::Error::Database(ref database) = error else {
            return Self::Database(error);
        };
        let Some(code) = database.code() else {
            return Self::Database(error);
        };
        let constraint = database.constraint().unwrap_or_default().to_owned();
        match &*code {
            "23505" => Self::Unique { constraint },
            "23503" => Self::ForeignKey { constraint },
            "23514" => Self::Check { constraint },
            "23502" => Self::NotNull {
                column: not_null_column(database.as_ref()),
            },
            _ => Self::Database(error),
        }
    }
}

/// Pull the offending column from a not-null violation (Postgres-specific).
fn not_null_column(database: &dyn DatabaseError) -> String {
    database
        .try_downcast_ref::<sqlx::postgres::PgDatabaseError>()
        .and_then(|pg| pg.column())
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::Error;

    #[test]
    fn classifiers_match_variants() {
        assert!(
            Error::Unique {
                constraint: "c".into()
            }
            .is_unique()
        );
        assert!(
            Error::ForeignKey {
                constraint: "c".into()
            }
            .is_foreign_key()
        );
        assert!(Error::NotFound.is_not_found());
        assert!(!Error::NotFound.is_unique());
    }

    #[test]
    fn row_not_found_maps_to_not_found() {
        let mapped: Error = sqlx::Error::RowNotFound.into();
        assert!(mapped.is_not_found());
    }
}
