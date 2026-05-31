//! [`TSType`] impls for container / reference / generic types.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::TSType;

// --- references / smart pointers: transparent to the pointee ---

impl<T: TSType + ?Sized> TSType for &T {
    fn to_ts() -> String {
        T::to_ts()
    }
}

impl<T: TSType + ?Sized> TSType for &mut T {
    fn to_ts() -> String {
        T::to_ts()
    }
}

impl<T: TSType + ?Sized> TSType for Box<T> {
    fn to_ts() -> String {
        T::to_ts()
    }
}

// --- option -> `T | undefined` ---

impl<T: TSType> TSType for Option<T> {
    fn to_ts() -> String {
        format!("{} | undefined", T::to_ts())
    }
}

// --- sequences -> `Array<T>` ---

impl<T: TSType> TSType for Vec<T> {
    fn to_ts() -> String {
        format!("Array<{}>", T::to_ts())
    }
}

impl<T: TSType> TSType for [T] {
    fn to_ts() -> String {
        format!("Array<{}>", T::to_ts())
    }
}

impl<T: TSType, const N: usize> TSType for [T; N] {
    fn to_ts() -> String {
        format!("Array<{}>", T::to_ts())
    }
}

impl<T: TSType, S> TSType for HashSet<T, S> {
    fn to_ts() -> String {
        format!("Array<{}>", T::to_ts())
    }
}

impl<T: TSType> TSType for BTreeSet<T> {
    fn to_ts() -> String {
        format!("Array<{}>", T::to_ts())
    }
}

// --- maps -> `Record<K, V>` ---

impl<K: TSType, V: TSType, S> TSType for HashMap<K, V, S> {
    fn to_ts() -> String {
        format!("Record<{}, {}>", K::to_ts(), V::to_ts())
    }
}

impl<K: TSType, V: TSType> TSType for BTreeMap<K, V> {
    fn to_ts() -> String {
        format!("Record<{}, {}>", K::to_ts(), V::to_ts())
    }
}

impl<K: TSType, V: TSType, S> TSType for hashbrown::HashMap<K, V, S> {
    fn to_ts() -> String {
        format!("Record<{}, {}>", K::to_ts(), V::to_ts())
    }
}

impl<K: TSType, V: TSType, S> TSType for indexmap::IndexMap<K, V, S> {
    fn to_ts() -> String {
        format!("Record<{}, {}>", K::to_ts(), V::to_ts())
    }
}

// --- tuples -> `[A, B, …]` ---

macro_rules! ts_tuple {
    ($($name:ident),+) => {
        impl<$($name: TSType),+> TSType for ($($name,)+) {
            fn to_ts() -> String {
                let parts = [$($name::to_ts()),+];
                format!("[{}]", parts.join(", "))
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
