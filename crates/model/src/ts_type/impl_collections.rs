//! [`TSType`] impls for container / reference / generic types.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::TSType;

/// Shorthand for the declaration map the trait threads around.
type Decls = BTreeMap<String, String>;

// --- references / smart pointers: transparent to the pointee ---

impl<T: TSType + ?Sized> TSType for &T {
    fn ts_ref() -> String {
        T::ts_ref()
    }
    fn ts_declarations(out: &mut Decls) {
        T::ts_declarations(out);
    }
}

impl<T: TSType + ?Sized> TSType for &mut T {
    fn ts_ref() -> String {
        T::ts_ref()
    }
    fn ts_declarations(out: &mut Decls) {
        T::ts_declarations(out);
    }
}

impl<T: TSType + ?Sized> TSType for Box<T> {
    fn ts_ref() -> String {
        T::ts_ref()
    }
    fn ts_declarations(out: &mut Decls) {
        T::ts_declarations(out);
    }
}

// --- option -> `T | undefined` ---

impl<T: TSType> TSType for Option<T> {
    fn ts_ref() -> String {
        format!("{} | undefined", T::ts_ref())
    }
    fn ts_declarations(out: &mut Decls) {
        T::ts_declarations(out);
    }
}

// --- sequences -> `Array<T>` ---

macro_rules! ts_seq {
    ($($ty:ty),* $(,)?) => {
        $(
            impl<T: TSType> TSType for $ty {
                fn ts_ref() -> String {
                    format!("Array<{}>", T::ts_ref())
                }
                fn ts_declarations(out: &mut Decls) {
                    T::ts_declarations(out);
                }
            }
        )*
    };
}

ts_seq!(Vec<T>, [T], BTreeSet<T>);

impl<T: TSType, const N: usize> TSType for [T; N] {
    fn ts_ref() -> String {
        format!("Array<{}>", T::ts_ref())
    }
    fn ts_declarations(out: &mut Decls) {
        T::ts_declarations(out);
    }
}

impl<T: TSType, S> TSType for HashSet<T, S> {
    fn ts_ref() -> String {
        format!("Array<{}>", T::ts_ref())
    }
    fn ts_declarations(out: &mut Decls) {
        T::ts_declarations(out);
    }
}

// --- maps -> `Record<K, V>` ---

macro_rules! ts_map {
    ($($ty:ty),* $(,)?) => {
        $(
            impl<K: TSType, V: TSType, S> TSType for $ty {
                fn ts_ref() -> String {
                    format!("Record<{}, {}>", K::ts_ref(), V::ts_ref())
                }
                fn ts_declarations(out: &mut Decls) {
                    K::ts_declarations(out);
                    V::ts_declarations(out);
                }
            }
        )*
    };
}

ts_map!(HashMap<K, V, S>, hashbrown::HashMap<K, V, S>, indexmap::IndexMap<K, V, S>);

impl<K: TSType, V: TSType> TSType for BTreeMap<K, V> {
    fn ts_ref() -> String {
        format!("Record<{}, {}>", K::ts_ref(), V::ts_ref())
    }
    fn ts_declarations(out: &mut Decls) {
        K::ts_declarations(out);
        V::ts_declarations(out);
    }
}

// --- tuples -> `[A, B, …]` ---

macro_rules! ts_tuple {
    ($($name:ident),+) => {
        impl<$($name: TSType),+> TSType for ($($name,)+) {
            fn ts_ref() -> String {
                let parts = [$($name::ts_ref()),+];
                format!("[{}]", parts.join(", "))
            }
            fn ts_declarations(out: &mut Decls) {
                $($name::ts_declarations(out);)+
            }
        }
    };
}

ts_tuple!(A);
ts_tuple!(A, B);
ts_tuple!(A, B, C);
ts_tuple!(A, B, C, D);
ts_tuple!(A, B, C, D, E);
ts_tuple!(A, B, C, D, E, F);
ts_tuple!(A, B, C, D, E, F, G);
ts_tuple!(A, B, C, D, E, F, G, H);
