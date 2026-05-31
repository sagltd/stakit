//! Regex pattern rule.
//!
//! Regexes are compiled once (the derive emits a `LazyLock<Regex>` static per
//! pattern) and reused, so matching is allocation-free on the hot path.

pub use regex::Regex;

use crate::validate::error::ValidationError;

/// Value must match the (pre-compiled) regular expression.
///
/// # Errors
/// Fails with code `pattern` if `value` does not match `re`.
#[inline]
pub fn pattern(value: &str, re: &Regex) -> Result<(), ValidationError> {
    if re.is_match(value) {
        Ok(())
    } else {
        Err(ValidationError::new(
            "pattern",
            "does not match the required pattern",
        ))
    }
}
