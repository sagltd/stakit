//! SQL assembly: a single growing string plus an ordered, typed bind buffer.
//!
//! The whole statement is assembled once (at a terminal), never per builder
//! step, and `$N` placeholders are numbered globally in clause order.

use crate::ident::{self, IdentError};
use sqlx::error::BoxDynError;
use sqlx::postgres::PgArguments;
use sqlx::{Arguments, Encode, Postgres, Type};

/// A value queued to bind into a Postgres statement.
///
/// Boxed so heterogeneous value types share one ordered buffer; the concrete
/// type is preserved until it is handed to sqlx, so binding stays type-safe (no
/// string interpolation).
pub trait Bind: Send {
    /// Consume the boxed value and append it to sqlx arguments.
    ///
    /// # Errors
    /// Propagates any sqlx encode error.
    fn add(self: Box<Self>, args: &mut PgArguments) -> Result<(), BoxDynError>;
}

impl<T> Bind for T
where
    T: for<'q> Encode<'q, Postgres> + Type<Postgres> + Send + 'static,
{
    fn add(self: Box<Self>, args: &mut PgArguments) -> Result<(), BoxDynError> {
        args.add(*self)
    }
}

/// Inline-capacity bind buffer: most statements bind few values, so the common
/// case stays on the stack (no heap allocation for the buffer itself).
pub(crate) type BindBuffer = smallvec::SmallVec<[Box<dyn Bind>; 4]>;

/// Typical assembled statement length; pre-sizing avoids `String` reallocs in
/// the build hot path.
const DEFAULT_SQL_CAPACITY: usize = 96;

/// Accumulates SQL text and its ordered bind values.
pub struct SqlWriter {
    sql: String,
    binds: BindBuffer,
}

impl Default for SqlWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl SqlWriter {
    /// Create a writer with a pre-sized SQL buffer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sql: String::with_capacity(DEFAULT_SQL_CAPACITY),
            binds: BindBuffer::new(),
        }
    }

    /// Append raw SQL text (keywords, punctuation — never user values).
    pub fn push(&mut self, text: &str) {
        self.sql.push_str(text);
    }

    /// Append a single quoted identifier.
    ///
    /// # Errors
    /// Returns [`IdentError`] if the identifier is invalid.
    pub fn push_ident(&mut self, name: &str) -> Result<(), IdentError> {
        ident::write_quoted(&mut self.sql, name)
    }

    /// Append a table-qualified column: `"table"."column"`.
    ///
    /// # Errors
    /// Returns [`IdentError`] if either identifier is invalid.
    pub fn push_qualified(&mut self, table: &str, column: &str) -> Result<(), IdentError> {
        ident::write_quoted(&mut self.sql, table)?;
        self.sql.push('.');
        ident::write_quoted(&mut self.sql, column)
    }

    /// Queue a bind value and write its positional `$N` placeholder.
    pub fn push_bind(&mut self, value: Box<dyn Bind>) {
        self.binds.push(value);
        let position = self.binds.len();
        self.sql.push('$');
        // Avoid `format!` allocation in the hot path.
        let mut buffer = itoa_buffer();
        self.sql.push_str(itoa(position, &mut buffer));
    }

    /// Number of queued binds (also the next `$N`).
    #[must_use]
    pub fn bind_count(&self) -> usize {
        self.binds.len()
    }

    /// Borrow the assembled SQL text.
    #[must_use]
    pub fn sql(&self) -> &str {
        &self.sql
    }

    /// Consume into assembled SQL and its bind buffer.
    #[must_use]
    pub fn into_parts(self) -> (String, BindBuffer) {
        (self.sql, self.binds)
    }
}

/// A stack buffer large enough for any `usize` rendered as decimal.
type ItoaBuffer = [u8; 20];

const fn itoa_buffer() -> ItoaBuffer {
    [0; 20]
}

/// Render `value` into `buffer` and return the decimal substring.
fn itoa(value: usize, buffer: &mut ItoaBuffer) -> &str {
    if value == 0 {
        buffer[0] = b'0';
        return core::str::from_utf8(&buffer[..1]).unwrap_or("0");
    }
    let mut index = buffer.len();
    let mut remaining = value;
    while remaining > 0 {
        index -= 1;
        buffer[index] = b'0' + u8::try_from(remaining % 10).unwrap_or(0);
        remaining /= 10;
    }
    core::str::from_utf8(&buffer[index..]).unwrap_or("0")
}

#[cfg(test)]
mod tests {
    use super::SqlWriter;

    #[test]
    fn qualified_column_is_quoted() {
        let mut writer = SqlWriter::new();
        writer.push_qualified("users", "id").unwrap();
        assert_eq!(writer.sql(), r#""users"."id""#);
    }

    #[test]
    fn binds_are_numbered_in_order() {
        let mut writer = SqlWriter::new();
        writer.push("a = ");
        writer.push_bind(Box::new(10_i32));
        writer.push(" and b = ");
        writer.push_bind(Box::new(20_i32));
        assert_eq!(writer.sql(), "a = $1 and b = $2");
        assert_eq!(writer.bind_count(), 2);
    }

    #[test]
    fn itoa_renders_multi_digit() {
        let mut writer = SqlWriter::new();
        for _ in 0..12 {
            writer.push_bind(Box::new(1_i32));
            writer.push(" ");
        }
        assert!(writer.sql().contains("$12"));
    }
}
