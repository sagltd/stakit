//! Shared finalization: turn a [`SqlWriter`] into SQL text + backend-neutral
//! bind values. Converting values to a driver's native parameter type happens in
//! that driver (see [`crate::exec`]), keeping the builder backend-agnostic.

use crate::error::{Error, Result};
use crate::sql::{BindBuffer, SqlWriter};

/// Consume a writer into `(sql, values)`.
///
/// # Errors
/// Returns [`Error::Unsupported`] if a clause needed a backend feature this
/// dialect lacks (e.g. a `PostGIS` `ST_*` / `<->` operator on a non-Postgres
/// backend) — caught here so the caller never dispatches invalid SQL.
pub(crate) fn finish(writer: SqlWriter) -> Result<(String, BindBuffer)> {
    if let Some(feature) = writer.unsupported() {
        return Err(Error::Unsupported(feature));
    }
    Ok(writer.into_parts())
}
