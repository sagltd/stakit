//! String validation rules. All take `&str` and return a single
//! [`ValidationError`] on failure (path-less; the caller attaches the path).

use crate::validate::error::ValidationError;

/// Character-count length must be within `[min, max]` (either bound optional).
///
/// # Errors
/// Fails with code `length` when the char count is below `min` or above `max`.
#[inline]
pub fn length(value: &str, min: Option<usize>, max: Option<usize>) -> Result<(), ValidationError> {
    let len = value.chars().count();
    if let Some(min) = min {
        if len < min {
            return Err(ValidationError::new(
                "length",
                format!("must be at least {min} character(s)"),
            ));
        }
    }
    if let Some(max) = max {
        if len > max {
            return Err(ValidationError::new(
                "length",
                format!("must be at most {max} character(s)"),
            ));
        }
    }
    Ok(())
}

/// Value must be ASCII-only.
///
/// # Errors
/// Fails with code `ascii` if any non-ASCII character is present.
#[inline]
pub fn ascii(value: &str) -> Result<(), ValidationError> {
    if value.is_ascii() {
        Ok(())
    } else {
        Err(ValidationError::new(
            "ascii",
            "must contain only ASCII characters",
        ))
    }
}

/// Value must be alphanumeric (no spaces or punctuation).
///
/// # Errors
/// Fails with code `alphanumeric` if any character is not alphanumeric.
#[inline]
pub fn alphanumeric(value: &str) -> Result<(), ValidationError> {
    if value.chars().all(char::is_alphanumeric) {
        Ok(())
    } else {
        Err(ValidationError::new(
            "alphanumeric",
            "must contain only alphanumeric characters",
        ))
    }
}

/// Value must contain `needle`.
///
/// # Errors
/// Fails with code `contains` if `needle` is absent.
#[inline]
pub fn contains(value: &str, needle: &str) -> Result<(), ValidationError> {
    if value.contains(needle) {
        Ok(())
    } else {
        Err(ValidationError::new(
            "contains",
            format!("must contain {needle:?}"),
        ))
    }
}

/// Value must start with `prefix`.
///
/// # Errors
/// Fails with code `prefix` if `value` does not start with `prefix`.
#[inline]
pub fn prefix(value: &str, prefix: &str) -> Result<(), ValidationError> {
    if value.starts_with(prefix) {
        Ok(())
    } else {
        Err(ValidationError::new(
            "prefix",
            format!("must start with {prefix:?}"),
        ))
    }
}

/// Value must end with `suffix`.
///
/// # Errors
/// Fails with code `suffix` if `value` does not end with `suffix`.
#[inline]
pub fn suffix(value: &str, suffix: &str) -> Result<(), ValidationError> {
    if value.ends_with(suffix) {
        Ok(())
    } else {
        Err(ValidationError::new(
            "suffix",
            format!("must end with {suffix:?}"),
        ))
    }
}
