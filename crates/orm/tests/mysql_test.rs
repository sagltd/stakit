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

#[derive(Table, Debug, Clone, PartialEq, Eq)]
#[table(name = "users")]
struct User {
    #[column(pk)]
    id: i64,
    #[column(unique)]
    email: String,
    name: String,
    age: i32,
}

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

    db.insert_many(vec![
        UserNew {
            id: 1,
            email: "alice@x.com".to_owned(),
            name: "Alice".to_owned(),
            age: 30,
        },
        UserNew {
            id: 2,
            email: "bob@x.com".to_owned(),
            name: "Bob".to_owned(),
            age: 25,
        },
    ])
    .exec()
    .await
    .expect("seed");

    let fetched = db
        .select(User::all())
        .from::<User>()
        .filter(eq(User::id, 1_i64))
        .one()
        .await
        .expect("select one")
        .expect("row present");
    assert_eq!(fetched.email, "alice@x.com");

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
    let count = db.select(User::all()).from::<User>().count().await.unwrap();
    assert_eq!(count, 2);

    // update + transaction rollback.
    db.update::<User>()
        .set(User::age, 31)
        .filter(eq(User::id, 1_i64))
        .exec()
        .await
        .unwrap();

    let result: Result<()> = db
        .transaction(|tx| async move {
            tx.insert(UserNew {
                id: 3,
                email: "carol@x.com".to_owned(),
                name: "Carol".to_owned(),
                age: 40,
            })
            .exec()
            .await?;
            Err(Error::Transaction("rollback"))
        })
        .await;
    assert!(result.is_err());
    assert_eq!(
        db.select(User::all()).from::<User>().count().await.unwrap(),
        2,
        "rolled-back insert must not persist"
    );

    // delete.
    let deleted = db
        .delete::<User>()
        .filter(eq(User::id, 2_i64))
        .exec()
        .await
        .unwrap();
    assert_eq!(deleted, 1);
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

// Dedicated tables (not the shared `users`) so this test is parallel-safe under
// nextest's default concurrency.
#[derive(Table, Debug, Clone)]
#[table(name = "mt_authors")]
#[allow(dead_code)]
struct MtAuthor {
    #[column(pk)]
    id: i64,
    name: String,
}

#[derive(Table, Debug)]
#[table(name = "mt_posts")]
#[allow(dead_code)]
struct MtPost {
    #[column(pk)]
    id: i64,
    author_id: i64,
    title: String,
}

#[tokio::test]
async fn mysql_relations() {
    let Ok(url) = std::env::var("MYSQL_URL") else {
        eprintln!("MYSQL_URL not set; skipping MySQL relations e2e");
        return;
    };
    let db = Db::connect_mysql(&url).await.expect("connect");
    db.raw("drop table if exists mt_posts").exec().await.ok();
    db.raw("drop table if exists mt_authors").exec().await.ok();
    db.raw("create table mt_authors (id bigint primary key, name varchar(255) not null)")
        .exec()
        .await
        .expect("authors");
    db.raw("create table mt_posts (id bigint primary key, author_id bigint not null, title varchar(255) not null)").exec().await.expect("posts");
    db.insert(MtAuthorNew {
        id: 1,
        name: "Ann".to_owned(),
    })
    .exec()
    .await
    .unwrap();
    db.insert_many(vec![
        MtPostNew {
            id: 1,
            author_id: 1,
            title: "p1".to_owned(),
        },
        MtPostNew {
            id: 2,
            author_id: 1,
            title: "p2".to_owned(),
        },
    ])
    .exec()
    .await
    .unwrap();

    let authors = db.find::<MtAuthor>().all().await.unwrap();
    let with_posts = db
        .load_has_many::<MtAuthor, MtPost, i64>(
            authors,
            MtPost::author_id,
            |a| a.id,
            |p| p.author_id,
        )
        .await
        .expect("has_many");
    assert_eq!(with_posts[0].1.len(), 2);

    let posts = db.find::<MtPost>().all().await.unwrap();
    let with_author = db
        .load_belongs_to::<MtPost, MtAuthor, i64>(posts, |p| p.author_id, MtAuthor::id, |a| a.id)
        .await
        .expect("belongs_to");
    assert_eq!(with_author[0].1.as_ref().unwrap().name, "Ann");
}

#[derive(DbEnum, Debug, Clone, Copy, PartialEq, Eq)]
enum MyKind {
    A,
    B,
}

#[derive(DbEnum, Debug, Clone, Copy, PartialEq, Eq)]
#[db_enum(int)]
enum MyRank {
    Low = 1,
    High = 9,
}

#[derive(Table, Debug)]
#[table(name = "mt_things")]
#[allow(dead_code)]
struct MtThing {
    #[column(pk)]
    id: i64,
    #[column(sql_type = "varchar(16)")]
    kind: MyKind,
    #[column(sql_type = "int")]
    rank: MyRank,
    at: chrono::DateTime<chrono::Utc>,
    local: chrono::NaiveDateTime,
    day: chrono::NaiveDate,
    alarm: chrono::NaiveTime,
    meta: serde_json::Value,
}

/// Enums (text + int), all chrono temporal types, and JSON against real `MySQL`.
#[tokio::test]
async fn mysql_enums_temporal_json() {
    use chrono::{NaiveDate, NaiveTime, TimeZone, Utc};
    let Ok(url) = std::env::var("MYSQL_URL") else {
        eprintln!("MYSQL_URL not set; skipping MySQL types e2e");
        return;
    };
    let db = Db::connect_mysql(&url).await.expect("connect");
    db.raw("drop table if exists mt_things").exec().await.ok();
    db.raw(
        "create table mt_things (id bigint primary key, kind varchar(16) not null, rank int not null, \
         at datetime not null, local datetime not null, day date not null, alarm time not null, meta json not null)",
    )
    .exec()
    .await
    .expect("create");

    let at = Utc.with_ymd_and_hms(2026, 6, 2, 8, 30, 0).unwrap();
    let day = NaiveDate::from_ymd_opt(1990, 1, 15).unwrap();
    let alarm = NaiveTime::from_hms_opt(8, 0, 0).unwrap();
    let local = day.and_hms_opt(8, 30, 0).unwrap();
    let meta = serde_json::json!({ "k": [1, 2, 3] });

    db.insert(MtThingNew {
        id: 1,
        kind: MyKind::B,
        rank: MyRank::High,
        at,
        local,
        day,
        alarm,
        meta: meta.clone(),
    })
    .exec()
    .await
    .expect("insert");

    let got = db.get::<MtThing>(1).one().await.unwrap().unwrap();
    assert_eq!(got.kind, MyKind::B);
    assert_eq!(got.rank, MyRank::High);
    assert_eq!(got.local, local);
    assert_eq!(got.day, day);
    assert_eq!(got.alarm, alarm);
    assert_eq!(got.meta, meta);

    let highs = db
        .find::<MtThing>()
        .filter(eq(MtThing::rank, MyRank::High))
        .all()
        .await
        .unwrap();
    assert_eq!(highs.len(), 1);
}
