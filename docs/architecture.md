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

### `Validate` + `Model` + errors

Validation is **our own**, garde-free, built for speed (see `docs/performance.md`
— ~62 M validations/sec, ~49× a garde-derive baseline).

```rust
pub trait Validate { fn validate(&self) -> Result<(), ValidationErrors>; }
pub trait Model: Validate + TSType {}          // blanket-impl umbrella
```
- Rule functions live in `mod validate` (`length`, `range`, `email`, `url`,
  `pattern`, `ascii`, `alphanumeric`, `contains`, `prefix`, `suffix`). Each is
  `#[inline]`, returns a single `ValidationError`, and is reusable + `?`-able on
  its own. The derive calls these exact functions.
- `ValidationError { path, code, message }`; `ValidationErrors` aggregates **all**
  failures. Backed by a `Vec` — `Vec::new()` doesn't allocate, so the happy path
  is allocation-free and the success `Result` stays thin (a `SmallVec` inline
  buffer measured *slower*, as it bloats every return).
- **Cascading** `Validate` impls for `Option`, `Vec`, arrays, slices, sets,
  `HashMap`/`BTreeMap`/`hashbrown`/`indexmap`, and tuples mean `#[validate(dive)]`
  recurses through arbitrary nesting (`Vec<HashMap<String, Inner>>`), tagging each
  error with its index/key path (`rows[0][home].n`).
- `ValidationErrors::field_errors()` returns a `BTreeMap<&str, Vec<&str>>` for a
  structured per-field view.

### File layout (no `mod.rs`)
```
src/lib.rs                     # root, re-exports, prelude
src/model.rs                   # Model umbrella trait + generate_typescript
src/ts_type.rs (+ ts_type/)    # TSType trait + impls
src/validate.rs                # Validate trait + rule re-exports
src/validate/error.rs          # ValidationError / ValidationErrors
src/validate/{string,range,email,url,pattern}.rs   # rule fns (+ *_test.rs)
src/validate/collections.rs    # cascading Validate impls
benches/validation.rs          # divan benchmarks
tests/                         # e2e derive tests
```

## `stakit-model-derive`

`#[derive(Model)]` reads flat `#[validate(...)]` attributes and emits:

1. **`impl Validate`** — direct, inlined. Destructure `self`, and per field emit
   ```rust
   if let Err(e) = validate::length(name, Some(3), Some(20)) { __errors.push(e.at_field("name")); }
   ```
   collecting **all** failures. `pattern` emits a `LazyLock<Regex>` static
   (compiled once). `dive` calls `Validate::validate(field)` and prefixes paths.
   Enums: `match self { Self::Variant { .. } => { …field rules… }, Self::Unit => {} }`.

   **Rule set (flat, easy DX):** `skip`, `min_len`/`max_len`, `min`/`max`,
   `email`, `url`, `ascii`, `alphanumeric`, `contains`/`prefix`/`suffix`,
   `pattern = "regex"`, `custom = fn` (`fn(&T) -> Result<(), ValidationError>`),
   `dive`. Fields with no `#[validate]` are not validated (still rendered in TS).

2. **`impl TSType`**:
   - **struct** → `export interface Name {\n  field: Ty;\n  opt?: Ty;\n}`.
     Each field type rendered via `<FieldTy as TSType>::to_ts()`; `Option<T>`
     fields render as optional (`field?: T`).
   - **enum, all unit variants** → `export type Name = "A" | "B" | "C";`.
   - **enum with data variants** → union mixing literals + inline objects, e.g.
     `enum UserType { Normal, Help { aha: String } }` →
     `export type UserType = "Normal" | { aha: string };`.
     Tuple variant with one field → the inner type; with N fields → `[a, b, …]`.

Generated code references `::stakit_model::validate::*` and the `Validate` trait
by absolute path, so a user crate depends on `stakit-model` only.

## Quality gates

`./code-check.sh` → `cargo fmt --check` + `clippy -D warnings` + build +
`cargo nextest run` + doctests. Benches via `cargo bench` (divan, `harness=false`).
Lints (workspace): `unsafe_code = "forbid"`, clippy `pedantic` + `nursery`.
All dependencies pinned to latest published versions.
