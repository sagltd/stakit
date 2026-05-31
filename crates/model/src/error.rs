//! Validation error type.

use std::collections::BTreeMap;

use thiserror::Error;

/// Error returned when a [`Model`](crate::Model) fails validation.
///
/// Wraps [`garde::Report`], whose `Display` prints each failing field path and
/// message. Use [`ModelError::field_errors`] for a structured per-field view.
#[derive(Debug, Error)]
pub enum ModelError {
    /// One or more `garde` validation rules failed.
    #[error("validation failed:\n{0}")]
    Invalid(#[from] garde::Report),
}

impl ModelError {
    /// Groups the validation failures by field path, mapping each path to its
    /// list of messages.
    ///
    /// # Examples
    /// ```
    /// use stakit_model::{Model, ModelError};
    ///
    /// #[derive(Model)]
    /// struct Account {
    ///     #[garde(length(min = 3))]
    ///     name: String,
    /// }
    ///
    /// let err: ModelError = Account { name: "x".into() }.validate_model().unwrap_err();
    /// let fields = err.field_errors();
    /// assert!(fields.contains_key("name"));
    /// ```
    #[must_use]
    pub fn field_errors(&self) -> BTreeMap<String, Vec<String>> {
        let Self::Invalid(report) = self;
        let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (path, error) in report.iter() {
            map.entry(path.to_string())
                .or_default()
                .push(error.message().to_owned());
        }
        map
    }
}
