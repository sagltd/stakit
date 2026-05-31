//! End-to-end tests for `#[derive(Model)]` on enums.
#![allow(dead_code)]

use stakit_model::{Model, Validate, generate_typescript};

#[derive(Model)]
enum Status {
    Active,
    Inactive,
    Pending,
}

#[derive(Model)]
enum UserType {
    Normal,
    Help { aha: String },
}

#[test]
fn unit_enum_is_string_literal_union() {
    assert_eq!(
        generate_typescript::<Status>(),
        r#"export type Status = "Active" | "Inactive" | "Pending";"#
    );
}

#[test]
fn data_enum_mixes_literal_and_object() {
    assert_eq!(
        generate_typescript::<UserType>(),
        r#"export type UserType = "Normal" | { aha: string };"#
    );
}

#[test]
fn unit_variant_validates_ok() {
    assert!(Status::Active.validate().is_ok());
}
