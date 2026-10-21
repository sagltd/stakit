//! Full login-budget and verification-resend query shapes shared by SQL and DB tests.
#![allow(dead_code, unreachable_pub)]
use chrono::{DateTime, Utc};
use stakit_orm::prelude::*;

#[derive(Table, Debug)]
#[table(name = "login_attempts")]
pub struct Attempt {
    #[column(pk)]
    pub id: String,
    #[column(unique)]
    pub identifier: String,
    #[column(default = "0")]
    pub failed_count: i32,
    #[column(default = "now()")]
    pub window_started_at: DateTime<Utc>,
}

#[derive(Table, Debug)]
#[table(name = "verifications", unique_index(user_purpose = (user_id, purpose)))]
pub struct Verification {
    #[column(pk)]
    pub id: String,
    pub user_id: String,
    pub purpose: String,
    pub method: String,
    pub code: String,
    #[column(nullable)]
    pub target: Option<String>,
    #[column(default = "now()")]
    pub created_at: DateTime<Utc>,
    #[column(default = "0")]
    pub tries: i32,
    #[column(default = "0")]
    pub resend_count: i32,
}

pub fn attempt_row(id: &str, identifier: &str) -> AttemptNew {
    AttemptNew {
        id: id.into(),
        identifier: identifier.into(),
        failed_count: Some(1),
        window_started_at: None,
    }
}

pub fn reserve(
    insert: stakit_orm::Insert<AttemptNew>,
    seconds: i64,
    max: i32,
) -> Upsert<AttemptNew> {
    let expired = || {
        lte(
            stored(Attempt::window_started_at),
            db_now_minus_seconds(seconds),
        )
    };
    insert
        .on_conflict(Attempt::identifier)
        .set_expr(
            Attempt::failed_count,
            case_when(expired(), 1_i32, add(stored(Attempt::failed_count), 1_i32)),
        )
        .set_expr(
            Attempt::window_started_at,
            case_when(expired(), db_now(), stored(Attempt::window_started_at)),
        )
        .where_conflict(or(expired(), lt(stored(Attempt::failed_count), max)))
}

pub fn verification_row(id: &str, code: &str) -> VerificationNew {
    VerificationNew {
        id: id.into(),
        user_id: "user".into(),
        purpose: "signin".into(),
        method: "email".into(),
        code: code.into(),
        target: Some("recipient@example.test".into()),
        created_at: None,
        tries: Some(0),
        resend_count: Some(0),
    }
}

pub fn resend(
    insert: stakit_orm::Insert<VerificationNew>,
    seconds: i64,
) -> Upsert<VerificationNew> {
    insert
        .on_conflict((Verification::user_id, Verification::purpose))
        .set_expr(Verification::method, excluded(Verification::method))
        .set_expr(Verification::code, excluded(Verification::code))
        .set_expr(Verification::target, excluded(Verification::target))
        .set_expr(Verification::tries, 0_i32)
        .set_expr(
            Verification::resend_count,
            case_when(
                lte(
                    stored(Verification::created_at),
                    db_now_minus_seconds(seconds),
                ),
                0_i32,
                add(stored(Verification::resend_count), 1_i32),
            ),
        )
        .set_expr(Verification::created_at, excluded(Verification::created_at))
}
