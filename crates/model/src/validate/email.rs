//! Email validation rule (lightweight structural check, allocation-free).

use crate::validate::error::ValidationError;

/// Value must look like an email address, with no whitespace or control characters.
///
/// Requires a non-empty local part, a single `@`, and a dotted non-empty domain.
/// Not a full RFC 5322 parser (it rejects the exotic quoted-local form that may
/// legally contain spaces) — fast and good enough for form/API input, and it
/// refuses the newline/CR that enables email header injection.
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
    // No spaces, tabs, newlines/CR, or other control characters (incl. Unicode
    // whitespace like NBSP) — invalid in an unquoted address, and a newline/CR is an
    // email-header-injection vector.
    if s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    let Some(at) = s.find('@') else {
        return false;
    };
    let local = &s[..at];
    let domain = &s[at + 1..];
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return false;
    }
    // Domain must be dotted, with no leading/trailing/empty labels (`a..b`).
    !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains("..")
        && domain.contains('.')
}
