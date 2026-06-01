//! The [`JsonSchema`] trait — maps a Rust type to a JSON Schema fragment.
//!
//! Used to produce the `parameters` / `input_schema` object that LLM
//! function-calling APIs (Anthropic, `OpenAI`, MCP) expect for a tool's
//! arguments. Like [`TSType`](crate::TSType), scalars and collections return an
//! inline fragment (`{"type":"string"}`, `{"type":"array","items":…}`) and a
//! `#[derive(JsonSchema)]` struct returns a full `{"type":"object", …}` schema.
//!
//! The output targets JSON Schema draft 2020-12 (the dialect accepted by the
//! function-calling APIs). Constraints declared via `#[validate(...)]` are
//! lowered to the matching schema keywords by the derive macro.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde_json::{Value, json};

/// Maps a Rust type to its JSON Schema fragment.
///
/// Implemented for the common scalar / collection types out of the box; derive
/// it (`#[derive(JsonSchema)]`) or implement it manually for your own types.
#[diagnostic::on_unimplemented(
    message = "`{Self}` has no JSON Schema",
    note = "derive `JsonSchema` or manually implement it for `{Self}`"
)]
pub trait JsonSchema {
    /// Returns the JSON Schema fragment describing this type.
    fn schema() -> Value;
}

/// The unit type maps to JSON `null` (param-less tools).
impl JsonSchema for () {
    fn schema() -> Value {
        json!({ "type": "null" })
    }
}

macro_rules! schema_scalar {
    ($($t:ty => $ty_name:literal),* $(,)?) => {
        $(
            impl JsonSchema for $t {
                fn schema() -> Value {
                    json!({ "type": $ty_name })
                }
            }
        )*
    };
}

schema_scalar! {
    i8 => "integer", i16 => "integer", i32 => "integer", i64 => "integer", i128 => "integer", isize => "integer",
    u8 => "integer", u16 => "integer", u32 => "integer", u64 => "integer", u128 => "integer", usize => "integer",
    f32 => "number", f64 => "number",
    bool => "boolean",
    char => "string", str => "string", String => "string",
}

// --- references / smart pointers: transparent to the pointee ---

impl<T: JsonSchema + ?Sized> JsonSchema for &T {
    fn schema() -> Value {
        T::schema()
    }
}

impl<T: JsonSchema + ?Sized> JsonSchema for &mut T {
    fn schema() -> Value {
        T::schema()
    }
}

impl<T: JsonSchema + ?Sized> JsonSchema for Box<T> {
    fn schema() -> Value {
        T::schema()
    }
}

// --- option: transparent. Optionality is expressed by `required` at the
// object level (the derive omits `Option<T>` fields from `required`). ---

impl<T: JsonSchema> JsonSchema for Option<T> {
    fn schema() -> Value {
        T::schema()
    }
}

// --- sequences -> `{"type":"array","items":T}` ---

macro_rules! schema_seq {
    ($($t:ty),* $(,)?) => {
        $(
            impl<T: JsonSchema> JsonSchema for $t {
                fn schema() -> Value {
                    json!({ "type": "array", "items": T::schema() })
                }
            }
        )*
    };
}

schema_seq!(Vec<T>, [T], BTreeSet<T>);

impl<T: JsonSchema, const N: usize> JsonSchema for [T; N] {
    fn schema() -> Value {
        json!({ "type": "array", "items": T::schema() })
    }
}

impl<T: JsonSchema, S> JsonSchema for HashSet<T, S> {
    fn schema() -> Value {
        json!({ "type": "array", "items": T::schema() })
    }
}

// --- maps -> `{"type":"object","additionalProperties":V}` ---

macro_rules! schema_map {
    ($($t:ty),* $(,)?) => {
        $(
            impl<K, V: JsonSchema, S> JsonSchema for $t {
                fn schema() -> Value {
                    json!({ "type": "object", "additionalProperties": V::schema() })
                }
            }
        )*
    };
}

schema_map!(HashMap<K, V, S>, hashbrown::HashMap<K, V, S>, indexmap::IndexMap<K, V, S>);

impl<K, V: JsonSchema> JsonSchema for BTreeMap<K, V> {
    fn schema() -> Value {
        json!({ "type": "object", "additionalProperties": V::schema() })
    }
}

// --- tuples -> `{"type":"array","prefixItems":[…]}` (fixed-length) ---

macro_rules! schema_tuple {
    ($($name:ident),+) => {
        impl<$($name: JsonSchema),+> JsonSchema for ($($name,)+) {
            fn schema() -> Value {
                let prefix = vec![$($name::schema()),+];
                let len = prefix.len();
                json!({ "type": "array", "prefixItems": prefix, "minItems": len, "maxItems": len })
            }
        }
    };
}

schema_tuple!(A);
schema_tuple!(A, B);
schema_tuple!(A, B, C);
schema_tuple!(A, B, C, D);
schema_tuple!(A, B, C, D, E);
schema_tuple!(A, B, C, D, E, F);
schema_tuple!(A, B, C, D, E, F, G);
schema_tuple!(A, B, C, D, E, F, G, H);
