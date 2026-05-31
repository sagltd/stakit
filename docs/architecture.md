# stakit — Architecture

Rust workspace. A backend/API toolkit with great DX. Edition 2024.

## Inspiration

Ported from **ggtype** (`/Users/samuelgja/Documents/PRIVATE/libs/ggtype`), a
TypeScript library whose builder-style models do runtime validation **and**
generate JSON Schema / infer TS types. stakit keeps the *idea* (one model
declaration drives validation + a TypeScript type for web clients) but flips the
ergonomics to idiomatic Rust: a `#[derive(Model)]` instead of a runtime builder,
backed by [`garde`](https://docs.rs/garde) for fast, compile-checked validation.

## Workspace roadmap

| Crate | Status | Purpose |
|-------|--------|---------|
| `crates/model` (`stakit-model`) | **now** | `TSType` trait, `Model` trait, error type, TS generation. |
| `crates/model-derive` (`stakit-model-derive`) | **now** | `#[derive(Model)]` proc-macro. |
| `crates/action` | later | RPC/handler layer. |
| `crates/router` | later | Routing over actions. |
| `crates/stakit` | later | Facade re-exporting everything — the single, easy-DX entrypoint. |

Each later crate gets its own spec before implementation.

## `stakit-model`

### `TSType`
```rust
#[diagnostic::on_unimplemented(message = "`{Self}` cannot be exported to TypeScript", ...)]
pub trait TSType {
    fn to_ts() -> String;
}
```
Single method. Contract:
- **Scalars / collections** return an *inline* TS type: `i32 → "number"`,
  `String → "string"`, `Vec<T> → "Array<{T}>"`, `Option<T> → "{T} | undefined"`,
  `HashMap<K,V>` / `IndexMap<K,V>` → `"Record<{K}, {V}>"`.
- **`#[derive(Model)]` struct** returns a full `export interface Name { … }`.
- **`#[derive(Model)]` enum** returns a TS union (see below).

Implemented out of the box for primitives, `String`/`&str`, `Option`, `Vec`,
arrays, tuples, `HashMap`/`BTreeMap`/`HashSet`, `hashbrown::HashMap`,
`indexmap::IndexMap`. A field type with no `TSType` impl → friendly compile
error via `#[diagnostic::on_unimplemented]`.

`pub fn generate_typescript<M: TSType>() -> String` wraps `M::to_ts()` and is the
discoverable entrypoint + future home of transitive multi-type emission.

> **Known limitation (v1):** a *derived struct used as a field of another* emits
> its full interface inline instead of a named reference. Transitive emission
> (collect referenced named types into one output) is a later step — see below.

### Type reuse / registry (planned, v-next)

Goal: minimal, non-repeating output where each named type is emitted **once** and
referenced by name elsewhere (mirrors ggtype's `modelsState` + JSON Schema
`$defs`). Planned shape, designed so it does not break `to_ts()`:

```rust
pub struct TsRegistry { /* name -> definition, insertion-ordered (indexmap) */ }
pub trait TSType {
    fn to_ts() -> String;                       // inline form (today)
    fn collect(_registry: &mut TsRegistry) {}   // register self + deps (v-next)
}
```
`generate_typescript::<Root>()` would then walk `collect`, dedupe by type name,
and emit `export interface`/`export type` blocks once, with fields referencing
named types by identifier. The derive already knows a type's name and its field
types, so it can generate `collect` later with no API churn. Not built in v1.

### `Model` + errors
```rust
pub trait Model: garde::Validate<Context = ()> + TSType {
    fn validate_model(&self) -> Result<(), ModelError>;
}
// blanket impl for everything that is Validate<Context=()> + TSType
```
`ModelError` (thiserror) wraps `garde::Report` (whose `Display` prints each
failing path + message). `garde` is re-exported (`stakit_model::garde`) so
downstream crates need only depend on `stakit-model`.

### File layout (no `mod.rs`)
```
src/lib.rs                     # root, re-exports, __private (for derive)
src/model.rs                   # Model trait + generate_typescript
src/error.rs                   # ModelError
src/ts_type.rs                 # TSType trait
src/ts_type/impl_primitives.rs # scalar impls
src/ts_type/impl_collections.rs# container / generic impls
benches/validation.rs          # divan benchmarks
tests/                         # integration + derive tests
```

## `stakit-model-derive`

`#[derive(Model)]` reads **native garde attributes** (`#[garde(...)]`) and emits:

1. **`impl garde::Validate`** — hand-generated (we do *not* use garde's own
   derive). Mirrors garde's codegen: destructure `self`, and per field emit
   ```rust
   { let mut __p = nested_path!(__p, "field");
     if let Err(e) = (rules::<rule>::apply)(&*binding, args) { report.append(__p(), e); } }
   ```
   Enums: `match self { Self::Variant { .. } => { …field rules… }, Self::Unit => {} }`.

   **v1 rule set:** `skip`, `length(min/max/equal)`, `range(min/max/equal)`,
   `email`, `url`, `ascii`, `alphanumeric`, `contains/prefix/suffix`,
   `pattern(regex literal)`, `custom(fn)`, `dive`. Fields with no `#[garde]`
   attribute default to **skip** (rendered in TS, not validated).
   Deferred: conditional `if`, custom `context`, `inner`, `transparent`, `ip`.

2. **`impl TSType`**:
   - **struct** → `export interface Name {\n  field: Ty;\n  opt?: Ty;\n}`.
     Each field type rendered via `<FieldTy as TSType>::to_ts()`; `Option<T>`
     fields render as optional (`field?: T`).
   - **enum, all unit variants** → `export type Name = "A" | "B" | "C";`.
   - **enum with data variants** → union mixing literals + inline objects, e.g.
     `enum UserType { Normal, Help { aha: String } }` →
     `export type UserType = "Normal" | { aha: string };`.
     Tuple variant with one field → the inner type; with N fields → `[a, b, …]`.

Generated code references `stakit-model`'s `__private` re-exports
(`::stakit_model::__private::…`), so a user crate depends on `stakit-model` only.

## Quality gates

`./code-check.sh` → `cargo fmt --check` + `clippy -D warnings` + build +
`cargo nextest run` + doctests. Benches via `cargo bench` (divan, `harness=false`).
Lints (workspace): `unsafe_code = "forbid"`, clippy `pedantic` + `nursery`.
All dependencies pinned to latest published versions.
