//! Email validation rule (lightweight structural check, allocation-free).

use crate::validate::error::ValidationError;

/// Value must look like an email address: a non-empty local part, a single `@`,
/// and a dotted, non-empty domain. Not a full RFC 5322 parser — fast and
/// good enough for form/API input.
///
/// # Errors
/// Fails with code `email` if the structure is invalid.
#[inline]
pub fn email(value: &str) -> Result<(), ValidationError> {
    if looks_like_email(value) {
        Ok(())
    } else {
        Err(ValidationError::new(
            "email",
            "must be a valid email address",
        ))
    }
}

fn looks_like_email(s: &str) -> bool {
    let Some(at) = s.find('@') else {
        return false;
    };
    let local = &s[..at];
    let domain = &s[at + 1..];
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return false;
    }
    !domain.starts_with('.') && !domain.ends_with('.') && domain.contains('.')
}
