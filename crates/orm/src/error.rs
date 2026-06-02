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
    /// An operation unsupported by the active backend (e.g. `RETURNING` on `MySQL`).
    #[error("operation not supported by this backend: {0}")]
    Unsupported(&'static str),
    /// An unclassified Turso / `libSQL` backend error — the concrete `libsql::Error`,
    /// not a boxed `dyn` (only present with the `turso` feature). Transparent — may
    /// carry a raw backend message, so log server-side, don't show clients.
    #[cfg(feature = "turso")]
    #[error(transparent)]
    Turso(libsql::Error),
    /// An unclassified sqlx error (Postgres / `SQLite` / `MySQL` — sqlx unifies them
    /// into one concrete `sqlx::Error`). Transparent — may carry a raw backend
    /// message, so log server-side, don't show clients.
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
        // Classify via sqlx's backend-neutral `ErrorKind`, which works across
        // Postgres, SQLite, and MySQL (SQLSTATE/extended-result-code differences
        // are normalized by the driver), instead of matching Postgres SQLSTATE.
        let constraint = database.constraint().unwrap_or_default().to_owned();
        match database.kind() {
            sqlx::error::ErrorKind::UniqueViolation => Self::Unique { constraint },
            sqlx::error::ErrorKind::ForeignKeyViolation => Self::ForeignKey { constraint },
            sqlx::error::ErrorKind::CheckViolation => Self::Check { constraint },
            sqlx::error::ErrorKind::NotNullViolation => Self::NotNull {
                column: not_null_column(database.as_ref()),
            },
            _ => Self::Database(error),
        }
    }
}

/// Pull the offending column from a not-null violation (Postgres-specific).
#[cfg(feature = "postgres")]
fn not_null_column(database: &dyn DatabaseError) -> String {
    database
        .try_downcast_ref::<sqlx::postgres::PgDatabaseError>()
        .and_then(|pg| pg.column())
        .unwrap_or_default()
        .to_owned()
}

/// Without the Postgres backend there is no SQLSTATE column extraction.
#[cfg(not(feature = "postgres"))]
fn not_null_column(_database: &dyn DatabaseError) -> String {
    String::new()
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

    #[test]
    fn classifiers_are_mutually_exclusive() {
        let unique = Error::Unique {
            constraint: "c".into(),
        };
        assert!(!unique.is_foreign_key());
        assert!(!unique.is_not_found());

        let fk = Error::ForeignKey {
            constraint: "c".into(),
        };
        assert!(!fk.is_unique());
        assert!(!fk.is_not_found());

        assert!(!Error::NotFound.is_foreign_key());
    }

    #[test]
    fn ident_error_converts_into_error() {
        let error: Error = crate::ident::IdentError::Empty.into();
        assert!(matches!(
            error,
            Error::Ident(crate::ident::IdentError::Empty)
        ));
    }

    #[test]
    fn display_messages_render() {
        assert_eq!(Error::NotFound.to_string(), "not found");
        assert_eq!(
            Error::TooManyRows.to_string(),
            "too many rows: expected one"
        );
        assert_eq!(
            Error::Unique {
                constraint: "users_email_key".into()
            }
            .to_string(),
            "unique violation on users_email_key"
        );
        assert_eq!(
            Error::ForeignKey {
                constraint: "posts_author_fk".into()
            }
            .to_string(),
            "foreign key violation on posts_author_fk"
        );
        assert_eq!(
            Error::NotNull {
                column: "email".into()
            }
            .to_string(),
            "not-null violation on email"
        );
        assert_eq!(
            Error::Check {
                constraint: "age_chk".into()
            }
            .to_string(),
            "check violation on age_chk"
        );
        assert_eq!(
            Error::Transaction("handle escaped").to_string(),
            "transaction misuse: handle escaped"
        );
    }

    #[test]
    fn non_database_sqlx_error_passes_through() {
        let mapped: Error = sqlx::Error::PoolClosed.into();
        assert!(matches!(mapped, Error::Database(_)));
        assert!(!mapped.is_unique());
    }
}
