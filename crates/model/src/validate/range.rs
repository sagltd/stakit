//! Numeric / ordered-value range rules.

use std::fmt::Display;

use crate::validate::error::ValidationError;

/// Value must be within `[min, max]` (either bound optional, inclusive).
///
/// Works for any `PartialOrd + Display` type (integers, floats, chars, …).
///
/// # Errors
/// Fails with code `range` when `value` is below `min` or above `max`.
#[inline]
#[expect(
    clippy::needless_pass_by_value,
    reason = "bounds are cheap Copy numerics; by-value keeps call sites clean"
)]
pub fn range<T>(value: &T, min: Option<T>, max: Option<T>) -> Result<(), ValidationError>
where
    T: PartialOrd + Display,
{
    if let Some(min) = &min {
        if value < min {
            return Err(ValidationError::new("range", format!("must be >= {min}")));
        }
    }
    if let Some(max) = &max {
        if value > max {
            return Err(ValidationError::new("range", format!("must be <= {max}")));
        }
    }
    Ok(())
}
