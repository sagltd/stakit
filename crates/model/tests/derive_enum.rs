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

// ── serde enum attributes: the TypeScript must match the serde wire ──────────

#[derive(Model, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum Role {
    Owner,
    TeamMember,
}

#[derive(Model, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum Frame {
    AgentStart { id: String },
    Heartbeat,
}

#[derive(Model, serde::Serialize, serde::Deserialize)]
enum Renamed {
    #[serde(rename = "first-one")]
    First,
    Second,
}

#[test]
fn rename_all_renames_unit_variants_like_serde() {
    assert_eq!(
        generate_typescript::<Role>(),
        r#"export type Role = "owner" | "team_member";"#
    );
    assert_eq!(
        serde_json::to_string(&Role::TeamMember).unwrap(),
        r#""team_member""#
    );
}

#[test]
fn internally_tagged_enum_carries_its_tag_in_every_variant() {
    assert_eq!(
        generate_typescript::<Frame>(),
        r#"export type Frame = { kind: "agentStart"; id: string } | { kind: "heartbeat" };"#
    );
    let json = serde_json::to_string(&Frame::AgentStart { id: "a".to_owned() }).unwrap();
    assert_eq!(json, r#"{"kind":"agentStart","id":"a"}"#);
    assert_eq!(
        serde_json::to_string(&Frame::Heartbeat).unwrap(),
        r#"{"kind":"heartbeat"}"#
    );
}

#[test]
fn variant_rename_wins_over_the_ident() {
    assert_eq!(
        generate_typescript::<Renamed>(),
        r#"export type Renamed = "first-one" | "Second";"#
    );
}
