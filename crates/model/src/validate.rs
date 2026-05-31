//! Fast, inlined validation.
//!
//! The [`Validate`] trait + a flat set of reusable rule functions
//! ([`length`], [`range`], [`email`], …). The functions are `#[inline]` and
//! return a single [`ValidationError`]; `#[derive(Model)]` calls these exact
//! functions, so derived and hand-written validation share one implementation.
//!
//! The happy path allocates nothing: rules return `Ok(())`, and the aggregate
//! [`ValidationErrors`] is a stack-inline [`smallvec::SmallVec`].

mod collections;
mod email;
mod error;
mod pattern;
mod range;
mod string;
mod url;

#[cfg(test)]
mod validate_test;

pub use email::email;
pub use error::{ValidationError, ValidationErrors};
pub use pattern::{Regex, pattern};
pub use range::range;
pub use string::{alphanumeric, ascii, contains, length, prefix, suffix};
pub use url::url;

/// A type that can validate itself, aggregating **all** failures.
///
/// Derived by `#[derive(Model)]`; implemented for common containers so
/// validation cascades through nested structures.
pub trait Validate {
    /// Validates `self`, collecting every failing rule.
    ///
    /// # Errors
    /// Returns [`ValidationErrors`] (non-empty) if any rule fails.
    fn validate(&self) -> Result<(), ValidationErrors>;
}
