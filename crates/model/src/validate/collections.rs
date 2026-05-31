//! Cascading [`Validate`] impls for containers.
//!
//! These let validation recurse through arbitrary nesting — `Vec<T>`,
//! `Option<T>`, maps, sets, and any combination (`Vec<HashMap<String, Inner>>`)
//! — as long as the leaf type implements [`Validate`] (i.e. derives `Model`).
//! Container elements/values contribute their index/key to the error path.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Display;

use crate::validate::Validate;
use crate::validate::error::ValidationErrors;

// --- transparent wrappers ---

impl<T: Validate + ?Sized> Validate for &T {
    fn validate(&self) -> Result<(), ValidationErrors> {
        T::validate(self)
    }
}

impl<T: Validate + ?Sized> Validate for Box<T> {
    fn validate(&self) -> Result<(), ValidationErrors> {
        T::validate(self)
    }
}

impl Validate for () {
    fn validate(&self) -> Result<(), ValidationErrors> {
        Ok(())
    }
}

impl<T: Validate> Validate for Option<T> {
    fn validate(&self) -> Result<(), ValidationErrors> {
        self.as_ref().map_or(Ok(()), Validate::validate)
    }
}

// --- sequences / sets: prefix child paths with `[index]` ---

fn validate_indexed<'a, T>(iter: impl IntoIterator<Item = &'a T>) -> Result<(), ValidationErrors>
where
    T: Validate + 'a,
{
    let mut errors = ValidationErrors::new();
    for (index, item) in iter.into_iter().enumerate() {
        if let Err(child) = item.validate() {
            errors.extend(child.into_iter().map(|e| e.at_index(index)));
        }
    }
    errors.into_result()
}

impl<T: Validate> Validate for Vec<T> {
    fn validate(&self) -> Result<(), ValidationErrors> {
        validate_indexed(self)
    }
}

impl<T: Validate> Validate for [T] {
    fn validate(&self) -> Result<(), ValidationErrors> {
        validate_indexed(self)
    }
}

impl<T: Validate, const N: usize> Validate for [T; N] {
    fn validate(&self) -> Result<(), ValidationErrors> {
        validate_indexed(self)
    }
}

impl<T: Validate, S> Validate for HashSet<T, S> {
    fn validate(&self) -> Result<(), ValidationErrors> {
        validate_indexed(self)
    }
}

impl<T: Validate> Validate for BTreeSet<T> {
    fn validate(&self) -> Result<(), ValidationErrors> {
        validate_indexed(self)
    }
}

// --- maps: prefix child paths with `[key]` ---

fn validate_keyed<'a, K, V>(
    iter: impl IntoIterator<Item = (&'a K, &'a V)>,
) -> Result<(), ValidationErrors>
where
    K: Display + 'a,
    V: Validate + 'a,
{
    let mut errors = ValidationErrors::new();
    for (key, value) in iter {
        if let Err(child) = value.validate() {
            let key = key.to_string();
            errors.extend(child.into_iter().map(|e| e.at_key(&key)));
        }
    }
    errors.into_result()
}

impl<K: Display, V: Validate, S> Validate for HashMap<K, V, S> {
    fn validate(&self) -> Result<(), ValidationErrors> {
        validate_keyed(self)
    }
}

impl<K: Display, V: Validate> Validate for BTreeMap<K, V> {
    fn validate(&self) -> Result<(), ValidationErrors> {
        validate_keyed(self)
    }
}

impl<K: Display, V: Validate, S> Validate for hashbrown::HashMap<K, V, S> {
    fn validate(&self) -> Result<(), ValidationErrors> {
        validate_keyed(self)
    }
}

impl<K: Display, V: Validate, S> Validate for indexmap::IndexMap<K, V, S> {
    fn validate(&self) -> Result<(), ValidationErrors> {
        validate_keyed(self)
    }
}

// --- tuples: prefix each element's path with its `[position]` ---

macro_rules! tuple_validate {
    ($($idx:tt : $T:ident),+) => {
        impl<$($T: Validate),+> Validate for ($($T,)+) {
            fn validate(&self) -> Result<(), ValidationErrors> {
                let mut errors = ValidationErrors::new();
                $(
                    if let Err(__child) = self.$idx.validate() {
                        errors.extend(__child.into_iter().map(|e| e.at_index($idx)));
                    }
                )+
                errors.into_result()
            }
        }
    };
}

tuple_validate!(0: A);
tuple_validate!(0: A, 1: B);
tuple_validate!(0: A, 1: B, 2: C);
tuple_validate!(0: A, 1: B, 2: C, 3: D);
tuple_validate!(0: A, 1: B, 2: C, 3: D, 4: E);
tuple_validate!(0: A, 1: B, 2: C, 3: D, 4: E, 5: F);
tuple_validate!(0: A, 1: B, 2: C, 3: D, 4: E, 5: F, 6: G);
tuple_validate!(0: A, 1: B, 2: C, 3: D, 4: E, 5: F, 6: G, 7: H);
