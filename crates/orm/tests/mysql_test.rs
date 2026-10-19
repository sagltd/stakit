#![allow(missing_docs)] // gated-out builds compile to an empty crate
#![cfg(feature = "mysql")]
//! Integration test against a real **`MySQL`** server. Unlike Postgres (embedded)
//! and `SQLite`/Turso (in-process), `MySQL` has no in-process mode, so this test
//! is **gated on the `MYSQL_URL` env var** and skips when it is absent — set it to
//! a throwaway database (e.g. in CI with a `MySQL` service) to run it.
//!
//! Proves the agnostic core runs on `MySQL`: schema DDL, `insert` / `insert_many`,
//! typed select / partial projection / `IN` membership, aggregates, update,
//! delete, and transactions. (`MySQL` has no `RETURNING`, so that is exercised by
//! the Postgres and `SQLite` suites instead.)

use stakit_orm::prelude::*;

#[path = "support/users.rs"]
mod users;
use users::{User, user};

#[tokio::test]
async fn end_to_end_against_mysql() {
    let Ok(url) = std::env::var("MYSQL_URL") else {
        eprintln!("MYSQL_URL not set; skipping MySQL e2e");
        return;
    };

    let db = Db::connect_mysql(&url).await.expect("connect mysql");

    // Fresh schema.
    db.raw("drop table if exists users")
        .exec()
        .await
        .expect("drop");
    db.raw(
        "create table users (\
            id bigint primary key, \
            email varchar(255) not null unique, \
            name varchar(255) not null, \
            age int not null)",
    )
    .exec()
    .await
    .expect("create table");

    users::seed_alice_bob(&db).await;
    users::select_alice(&db).await;

    // any_of -> IN (?, ?) and ordering.
    let some = db
        .select(User::all())
        .from::<User>()
        .filter(any_of(User::id, &[1_i64, 2]))
        .order_by(asc(User::age))
        .all()
        .await
        .unwrap();
    assert_eq!(some.len(), 2);
    assert_eq!(some[0].name, "Bob");

    // aggregate.
    assert_eq!(users::user_count(&db).await, 2);

    // update + transaction rollback.
    users::assert_updates_alice_age(&db).await;
    users::assert_failed_transaction_rolls_back(&db, user(3, "Carol", 40), 2).await;

    // delete.
    users::assert_deletes_bob(&db, 1).await;
}

#[tokio::test]
async fn mysql_migrations_run_out_of_box() {
    let Ok(url) = std::env::var("MYSQL_URL") else {
        eprintln!("MYSQL_URL not set; skipping MySQL migration e2e");
        return;
    };
    let db = Db::connect_mysql(&url).await.expect("connect");
    db.raw("drop table if exists gadgets").exec().await.ok();
    db.raw("drop table if exists _stakit_migrations")
        .exec()
        .await
        .ok();
    let migrations = [Migration {
        version: "0001",
        statements: &["create table gadgets (id bigint primary key, name varchar(64) not null)"],
    }];
    assert_eq!(db.migrate(&migrations).await.expect("migrate"), 1);
    assert_eq!(db.migrate(&migrations).await.expect("again"), 0);
}

#[path = "support/blog.rs"]
mod blog;

// Dedicated tables (not the shared `users`) so this test is parallel-safe under
// nextest's default concurrency.
#[tokio::test]
async fn mysql_relations() {
    let Ok(url) = std::env::var("MYSQL_URL") else {
        eprintln!("MYSQL_URL not set; skipping MySQL relations e2e");
        return;
    };
    let db = Db::connect_mysql(&url).await.expect("connect");
    db.raw("drop table if exists posts").exec().await.ok();
    db.raw("drop table if exists authors").exec().await.ok();
    blog::create_and_seed(
        &db,
        &[
            "create table authors (id bigint primary key, name varchar(255) not null, \
             country varchar(255))",
            "create table posts (id bigint primary key, author_id bigint not null, \
             title varchar(255) not null, views int not null)",
        ],
    )
    .await;
    blog::assert_has_many_and_belongs_to(&db).await;
}

#[path = "support/things.rs"]
mod things;

/// Enums (text + int), all chrono temporal types, and JSON against real `MySQL`.
#[tokio::test]
async fn mysql_enums_temporal_json() {
    let Ok(url) = std::env::var("MYSQL_URL") else {
        eprintln!("MYSQL_URL not set; skipping MySQL types e2e");
        return;
    };
    let db = Db::connect_mysql(&url).await.expect("connect");
    db.raw("drop table if exists things").exec().await.ok();
    db.raw(
        "create table things (id bigint primary key, kind varchar(16) not null, rank int not null, \
         at datetime not null, local datetime not null, day date not null, alarm time not null, \
         meta json not null)",
    )
    .exec()
    .await
    .expect("create");
    things::assert_round_trips(&db).await;
}

// ----- composite-key upsert with coalesce against real MySQL (env-gated) -----
//
// Proves the `ON DUPLICATE KEY UPDATE col = values(col)` / `coalesce(values(col),
// col)` path against a live MySQL server: one row per (user_id, device_id), and
// a learned location survives a later NULL-location login. Skips without MYSQL_URL.

#[path = "support/devices.rs"]
mod devices;

#[tokio::test]
async fn upsert_composite_key_coalesce_remembers_device_on_mysql() {
    let Ok(url) = std::env::var("MYSQL_URL") else {
        eprintln!("MYSQL_URL not set; skipping MySQL upsert e2e");
        return;
    };
    let db = Db::connect_mysql(&url).await.expect("connect mysql");

    db.raw("drop table if exists login_devices")
        .exec()
        .await
        .expect("drop");
    db.raw(
        "create table login_devices (\
            id bigint primary key, \
            user_id bigint not null, \
            device_id varchar(255) not null, \
            platform varchar(255) not null, \
            location varchar(255), \
            unique key uq_user_device (user_id, device_id))",
    )
    .exec()
    .await
    .expect("create");
    devices::assert_remembers_device(&db).await;
}
