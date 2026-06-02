//! [`TSType`] impls for primitive / scalar types.

use crate::TSType;

/// The unit type maps to TypeScript `null` (used for param-less actions).
impl TSType for () {
    fn ts_ref() -> String {
        String::from("null")
    }
}

macro_rules! ts_scalar {
    ($($t:ty => $name:literal),* $(,)?) => {
        $(
            impl TSType for $t {
                fn ts_ref() -> String {
                    String::from($name)
                }
            }
        )*
    };
}

ts_scalar! {
    i8 => "number", i16 => "number", i32 => "number", i64 => "number", i128 => "number", isize => "number",
    u8 => "number", u16 => "number", u32 => "number", u64 => "number", u128 => "number", usize => "number",
    f32 => "number", f64 => "number",
    bool => "boolean",
    char => "string", str => "string", String => "string",
}
