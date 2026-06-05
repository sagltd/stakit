#![allow(missing_docs)] // gated-out builds compile to an empty crate
#![cfg(feature = "postgres")]
//! End-to-end test for **string-form / cross-crate foreign keys**
//! (`references = "table.column"`).
//!
//! `accounts` has **no** `#[derive(Table)]` in this crate — the foreign key from
//! `membership.account_id` is expressed purely as a string, the way an `agent` crate
//! would reference `server`'s `accounts` table without depending on `server`. This
//! test proves (1) the derive compiles and records the literal table/column in the
//! column metadata, and (2) the generated inline FK is enforced (cascade) by Postgres.

use sqlx::Row as _;
use stakit_orm::{OnDelete, Table as _};

#[derive(stakit_orm::Table, Debug)]
#[table(name = "membership")]
#[allow(dead_code)]
struct Membership {
    #[column(pk)]
    id: i64,
    #[column(references = "accounts.id", on_delete = "cascade")]
    account_id: i64,
}

#[test]
fn string_fk_is_recorded_in_column_metadata() {
    let account_id = Membership::COLUMNS
        .iter()
        .find(|column| column.name == "account_id")
        .expect("account_id column");
    let fk = account_id.references.expect("foreign key");
    assert_eq!(
        fk.table, "accounts",
        "literal table name from the string form"
    );
    assert_eq!(fk.column, "id");
    assert!(matches!(fk.on_delete, OnDelete::Cascade));
}

#[tokio::test]
async fn string_fk_is_enforced_by_postgres() {
    let mut postgres = postgresql_embedded::PostgreSQL::default();
    postgres.setup().await.expect("setup embedded postgres");
    postgres.start().await.expect("start embedded postgres");
    postgres
        .create_database("string_fk_test")
        .await
        .expect("create database");
    let url = postgres.settings().url("string_fk_test");
    let pool = sqlx::PgPool::connect(&url).await.expect("connect");

    // `accounts` exists out of band (no derive); `membership` references it inline by
    // the string FK exactly as `stakit-orm-cli` would emit it.
    for statement in [
        "create table accounts (id bigint primary key);",
        "create table membership (id bigint primary key, \
         account_id bigint not null references \"accounts\" (\"id\") on delete cascade);",
        "insert into accounts (id) values (1);",
        "insert into membership (id, account_id) values (10, 1);",
    ] {
        sqlx::query(statement)
            .execute(&pool)
            .await
            .unwrap_or_else(|error| panic!("apply `{statement}`: {error}"));
    }

    // FK enforced: a membership pointing at a missing account is rejected.
    let orphan = sqlx::query("insert into membership (id, account_id) values (11, 999);")
        .execute(&pool)
        .await;
    assert!(orphan.is_err(), "FK must reject a missing account");

    // ON DELETE CASCADE: deleting the account removes its membership.
    sqlx::query("delete from accounts where id = 1;")
        .execute(&pool)
        .await
        .expect("delete account");
    let remaining: i64 = sqlx::query("select count(*) as n from membership")
        .fetch_one(&pool)
        .await
        .expect("count")
        .get::<i64, _>("n");
    assert_eq!(remaining, 0, "cascade should remove the membership");

    postgres.stop().await.ok();
}
