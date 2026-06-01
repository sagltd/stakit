//! Validation error types.

use std::borrow::Cow;
use std::fmt;

use indexmap::IndexMap;

/// A single validation failure.
///
/// `path` is the dotted/indexed location of the offending value (e.g.
/// `address.zip`, `tags[2]`, `scores[home]`), built as the error bubbles up
/// through nested containers. `code` is a stable machine identifier (e.g.
/// `"length"`, `"email"`), `message` a human-readable description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    /// Location of the failing value. Empty for a top-level scalar.
    pub path: String,
    /// Stable rule identifier.
    pub code: &'static str,
    /// Human-readable message.
    pub message: Cow<'static, str>,
}

impl ValidationError {
    /// Creates an error with an empty path.
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            path: String::new(),
            code,
            message: message.into(),
        }
    }

    /// Prepends a named field segment (`name`, `name.rest`, `name[0]…`).
    #[must_use]
    pub fn at_field(mut self, name: &str) -> Self {
        self.path = if self.path.is_empty() {
            name.to_owned()
        } else if self.path.starts_with('[') {
            // e.g. diving into `Vec`: `rows` + `[0].x` -> `rows[0].x`
            format!("{name}{}", self.path)
        } else {
            format!("{name}.{}", self.path)
        };
        self
    }

    /// Prepends an index segment (`[i]`, `[i].rest`, `[i][j]`).
    #[must_use]
    pub fn at_index(mut self, index: usize) -> Self {
        self.path = prefix_bracket(&itoa(index), &self.path);
        self
    }

    /// Prepends a map-key segment (`[key]`, `[key].rest`).
    #[must_use]
    pub fn at_key(mut self, key: &str) -> Self {
        self.path = prefix_bracket(key, &self.path);
        self
    }
}

fn itoa(n: usize) -> String {
    n.to_string()
}

fn prefix_bracket(seg: &str, rest: &str) -> String {
    if rest.is_empty() {
        format!("[{seg}]")
    } else if rest.starts_with('[') {
        format!("[{seg}]{rest}")
    } else {
        format!("[{seg}].{rest}")
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            write!(f, "{}: {}", self.code, self.message)
        } else {
            write!(f, "{}: {} ({})", self.path, self.message, self.code)
        }
    }
}

impl std::error::Error for ValidationError {}

/// An aggregate of [`ValidationError`]s, returned by [`Validate`](crate::Validate).
///
/// Backed by a `Vec`: `Vec::new()` allocates nothing, so the **happy path is
/// allocation-free** and the success `Result` stays pointer-thin (a fat inline
/// buffer would bloat every `validate()` return). The heap is touched only when
/// a failure is actually recorded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValidationErrors(Vec<ValidationError>);

impl ValidationErrors {
    /// Creates an empty collection.
    #[must_use]
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    /// Appends one error.
    pub fn push(&mut self, error: ValidationError) {
        self.0.push(error);
    }

    /// Returns `true` if there are no errors.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Number of errors.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Iterates over the errors.
    pub fn iter(&self) -> std::slice::Iter<'_, ValidationError> {
        self.0.iter()
    }

    /// Converts to `Ok(())` when empty, otherwise `Err(self)`.
    ///
    /// # Errors
    /// Returns `Err(self)` if any error was collected.
    pub fn into_result(self) -> Result<(), Self> {
        if self.0.is_empty() { Ok(()) } else { Err(self) }
    }

    /// Groups the failures by field path, mapping each path to its messages.
    #[must_use]
    pub fn field_errors(&self) -> IndexMap<&str, Vec<&str>> {
        let mut map: IndexMap<&str, Vec<&str>> = IndexMap::new();
        for error in &self.0 {
            map.entry(error.path.as_str())
                .or_default()
                .push(&error.message);
        }
        map
    }
}

impl Extend<ValidationError> for ValidationErrors {
    fn extend<I: IntoIterator<Item = ValidationError>>(&mut self, iter: I) {
        self.0.extend(iter);
    }
}

impl IntoIterator for ValidationErrors {
    type Item = ValidationError;
    type IntoIter = std::vec::IntoIter<ValidationError>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a ValidationErrors {
    type Item = &'a ValidationError;
    type IntoIter = std::slice::Iter<'a, ValidationError>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl From<ValidationError> for ValidationErrors {
    fn from(error: ValidationError) -> Self {
        let mut errors = Self::new();
        errors.push(error);
        errors
    }
}

impl fmt::Display for ValidationErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "validation failed ({} error(s)):", self.0.len())?;
        for error in &self.0 {
            writeln!(f, "  - {error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationErrors {}
