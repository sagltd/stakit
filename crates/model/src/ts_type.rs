//! The [`TSType`] trait — maps a Rust type to its TypeScript representation.

mod impl_collections;
mod impl_primitives;

#[cfg(test)]
mod ts_type_test;

/// Maps a Rust type to TypeScript.
///
/// Two parts, so nested and generic types compose into valid output:
/// - [`ts_ref`](TSType::ts_ref) — the type *reference* used in a field/type
///   position: `"number"`, `"string"`, `"Array<User>"`, `"User"`, and for a
///   generic struct the concrete instantiation (e.g. `"MessageUser"`).
/// - [`ts_declarations`](TSType::ts_declarations) — registers the `export …`
///   block(s) for this type **and everything it transitively references**,
///   keyed by name (deduped). Scalars/collections add nothing of their own but
///   recurse into element types.
///
/// [`to_ts`](TSType::to_ts) (default) returns the full, self-contained
/// TypeScript: every declaration this type pulls in, in name order.
///
/// Implemented for the common standard / `hashbrown` / `indexmap` types out of
/// the box; derive `Model` (or implement this trait) for your own types.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be exported to TypeScript",
    note = "derive `Model` or manually implement `TSType` for `{Self}`"
)]
pub trait TSType {
    /// The TypeScript reference for this type (what appears in a field/arg).
    fn ts_ref() -> String;

    /// Registers the `export …` declaration(s) for this type and everything it
    /// transitively references into `out` (keyed by declared name, deduped).
    fn ts_declarations(_out: &mut std::collections::BTreeMap<String, String>) {}

    /// The full, self-contained TypeScript for this type: every declaration it
    /// pulls in (name order), or just the reference for a bare scalar.
    fn to_ts() -> String {
        let mut decls = std::collections::BTreeMap::new();
        Self::ts_declarations(&mut decls);
        if decls.is_empty() {
            Self::ts_ref()
        } else {
            decls.into_values().collect::<Vec<_>>().join("\n\n")
        }
    }
}
