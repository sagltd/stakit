//! URL validation rule (lightweight structural check, allocation-free).

use crate::validate::error::ValidationError;

/// Value must look like a URL: `scheme://host…` with a valid scheme and a
/// non-empty host. Not a full URL parser — fast and good enough for input.
///
/// # Errors
/// Fails with code `url` if the structure is invalid.
#[inline]
pub fn url(value: &str) -> Result<(), ValidationError> {
    if looks_like_url(value) {
        Ok(())
    } else {
        Err(ValidationError::new("url", "must be a valid URL"))
    }
}

fn looks_like_url(s: &str) -> bool {
    let Some(pos) = s.find("://") else {
        return false;
    };
    let scheme = &s[..pos];
    let scheme_ok = !scheme.is_empty()
        && scheme.as_bytes()[0].is_ascii_alphabetic()
        && scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'));
    if !scheme_ok {
        return false;
    }
    let rest = &s[pos + 3..];
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    !rest[..host_end].is_empty()
}
