//! The [`TSType`] trait — maps a Rust type to its TypeScript representation.

mod impl_collections;
mod impl_primitives;

#[cfg(test)]
mod ts_type_test;

/// Maps a Rust type to a TypeScript type string.
///
/// Scalars and collections return an *inline* type (`"number"`, `"string"`,
/// `"Array<string>"`, `"Record<string, number>"`). A `#[derive(Model)]` struct
/// returns a full `export interface … { … }` block.
///
/// Implemented for the common standard / `hashbrown` / `indexmap` types out of
/// the box; derive `Model` (or implement this trait) for your own types.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be exported to TypeScript",
    note = "derive `Model` or manually implement `TSType` for `{Self}`"
)]
pub trait TSType {
    /// Returns the TypeScript representation of this type.
    fn to_ts() -> String;
}
