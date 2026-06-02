//! Shared finalization: turn a [`SqlWriter`] into SQL text + backend-neutral
//! bind values. Converting values to a driver's native parameter type happens in
//! that driver (see [`crate::exec`]), keeping the builder backend-agnostic.

use crate::sql::{BindBuffer, SqlWriter};

/// Consume a writer into `(sql, values)`.
#[must_use]
pub(crate) fn finish(writer: SqlWriter) -> (String, BindBuffer) {
    writer.into_parts()
}
