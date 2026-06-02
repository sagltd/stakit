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

/// Append a quoted identifier to `out`.
///
/// # Errors
/// Returns [`IdentError`] if [`validate`] rejects `name`.
pub(crate) fn write_quoted(out: &mut String, name: &str) -> Result<(), IdentError> {
    validate(name)?;
    out.push('"');
    for ch in name.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{IdentError, MAX_IDENT_LEN, validate, write_quoted};

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
}
