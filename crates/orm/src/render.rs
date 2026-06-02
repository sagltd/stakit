//! Shared finalization: turn a [`SqlWriter`] into SQL text + bound arguments.

use crate::error::{Error, Result};
use crate::sql::SqlWriter;
use sqlx::postgres::PgArguments;

/// Consume a writer into `(sql, arguments)`, encoding all queued binds.
///
/// # Errors
/// Returns [`Error::Encode`] if a bind value fails to encode.
pub(crate) fn finish(writer: SqlWriter) -> Result<(String, PgArguments)> {
    let (sql, binds) = writer.into_parts();
    let mut arguments = PgArguments::default();
    for bind in binds {
        bind.add(&mut arguments).map_err(Error::Encode)?;
    }
    Ok((sql, arguments))
}
