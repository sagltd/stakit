//! Postgres identifier quoting and validation.
//!
//! Identifiers (table / column names) come only from compile-time schema tokens,
//! but are still rendered with correct quoting: wrapped in `"`, every embedded
//! `"` doubled, NUL rejected, and length checked against Postgres `NAMEDATALEN`.

/// Postgres truncates identifiers at this many bytes (`NAMEDATALEN - 1`). Two
/// names sharing a 63-byte prefix would silently collide, so we reject longer.
pub(crate) const MAX_IDENT_LEN: usize = 63;

/// Error returned when an identifier cannot be safely rendered.
#[derive(Debug, PartialEq, Eq)]
pub enum IdentError {
    /// The identifier was empty.
    Empty,
    /// The identifier contained a NUL byte (illegal in Postgres identifiers).
    ContainsNul,
    /// The identifier exceeded [`MAX_IDENT_LEN`] bytes.
    TooLong(usize),
}

impl core::fmt::Display for IdentError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Empty => f.write_str("identifier is empty"),
            Self::ContainsNul => f.write_str("identifier contains a NUL byte"),
            Self::TooLong(len) => write!(f, "identifier is {len} bytes, exceeds {MAX_IDENT_LEN}"),
        }
    }
}

impl core::error::Error for IdentError {}

/// Validate a raw identifier without quoting it.
///
/// # Errors
/// Returns [`IdentError`] if the name is empty, contains a NUL byte, or exceeds
/// [`MAX_IDENT_LEN`].
pub(crate) fn validate(name: &str) -> Result<(), IdentError> {
    if name.is_empty() {
        return Err(IdentError::Empty);
    }
    if name.as_bytes().contains(&0) {
        return Err(IdentError::ContainsNul);
    }
    if name.len() > MAX_IDENT_LEN {
        return Err(IdentError::TooLong(name.len()));
    }
    Ok(())
}

/// Append an identifier to `out`, quoted with `quote` (`"` for the SQL standard,
/// `` ` `` for `MySQL`); embedded `quote` characters are doubled.
///
/// # Errors
/// Returns [`IdentError`] if [`validate`] rejects `name`.
pub(crate) fn write_quoted_with(
    out: &mut String,
    name: &str,
    quote: char,
) -> Result<(), IdentError> {
    validate(name)?;
    out.reserve(name.len() + 2);
    out.push(quote);
    // Fast path: real schema identifiers never contain the quote char, so copy
    // the whole name in one `push_str` instead of char-by-char.
    if name.contains(quote) {
        for ch in name.chars() {
            if ch == quote {
                out.push(quote);
            }
            out.push(ch);
        }
    } else {
        out.push_str(name);
    }
    out.push(quote);
    Ok(())
}

/// Append a standard double-quoted identifier (Postgres / `SQLite` / Turso).
///
/// # Errors
/// Returns [`IdentError`] if [`validate`] rejects `name`.
#[cfg(test)]
pub(crate) fn write_quoted(out: &mut String, name: &str) -> Result<(), IdentError> {
    write_quoted_with(out, name, '"')
}

#[cfg(test)]
mod tests {
    use super::{IdentError, MAX_IDENT_LEN, validate, write_quoted, write_quoted_with};

    fn quote(name: &str) -> Result<String, IdentError> {
        let mut out = String::new();
        write_quoted(&mut out, name)?;
        Ok(out)
    }

    #[test]
    fn plain_name_is_wrapped() {
        assert_eq!(quote("users").unwrap(), r#""users""#);
    }

    #[test]
    fn mysql_backtick_quoting_doubles_embedded_backticks() {
        let mut out = String::new();
        write_quoted_with(&mut out, "ta`b", '`').unwrap();
        assert_eq!(out, "`ta``b`");
    }

    #[test]
    fn embedded_quote_is_doubled() {
        assert_eq!(quote(r#"a"b"#).unwrap(), r#""a""b""#);
    }

    #[test]
    fn nul_is_rejected() {
        assert_eq!(validate("a\0b"), Err(IdentError::ContainsNul));
    }

    #[test]
    fn empty_is_rejected() {
        assert_eq!(validate(""), Err(IdentError::Empty));
    }

    #[test]
    fn over_namedatalen_is_rejected() {
        let long = "x".repeat(MAX_IDENT_LEN + 1);
        assert_eq!(validate(&long), Err(IdentError::TooLong(MAX_IDENT_LEN + 1)));
    }

    #[test]
    fn at_namedatalen_is_allowed() {
        let ok = "x".repeat(MAX_IDENT_LEN);
        assert!(validate(&ok).is_ok());
    }

    #[test]
    fn write_quoted_rejects_empty_and_leaves_output_clean() {
        let mut out = String::new();
        assert_eq!(write_quoted(&mut out, ""), Err(IdentError::Empty));
        assert!(out.is_empty());
    }

    #[test]
    fn write_quoted_rejects_nul() {
        let mut out = String::new();
        assert_eq!(write_quoted(&mut out, "a\0b"), Err(IdentError::ContainsNul));
    }

    #[test]
    fn multiple_embedded_quotes_are_all_doubled() {
        assert_eq!(quote(r#"a"b"c"#).unwrap(), r#""a""b""c""#);
    }

    #[test]
    fn over_length_reports_actual_length() {
        let long = "x".repeat(MAX_IDENT_LEN + 5);
        assert_eq!(
            write_quoted(&mut String::new(), &long),
            Err(IdentError::TooLong(MAX_IDENT_LEN + 5))
        );
    }

    #[test]
    fn error_display_messages() {
        assert_eq!(IdentError::Empty.to_string(), "identifier is empty");
        assert_eq!(
            IdentError::ContainsNul.to_string(),
            "identifier contains a NUL byte"
        );
        assert_eq!(
            IdentError::TooLong(99).to_string(),
            format!("identifier is 99 bytes, exceeds {MAX_IDENT_LEN}")
        );
    }
}
