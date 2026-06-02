#![allow(missing_docs)] // gated-out builds compile to an empty crate
#![cfg(feature = "postgres")]
//! Integration test against a **real, embedded** Postgres (`postgresql_embedded`)
//! — no Docker. Boots a server in a temp dir, applies migrations via sqlx, then
//! exercises the typed query builder (select / update / delete) end to end.
//!
//! `postgresql_embedded` downloads a real Postgres binary on first run (cached
//! afterward), so this test needs network access the first time.

use futures::StreamExt as _;
use sqlx::migrate::Migrator;
use stakit_orm::prelude::*;
use uuid::Uuid;

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

#[derive(Table, Debug, Clone, PartialEq, Eq)]
#[table(name = "users")]
struct User {
    #[column(pk)]
    id: Uuid,
    #[column(unique)]
    email: String,
    name: String,
    // Added by the 0002 ALTER TABLE migration (default true).
    active: bool,
}

#[derive(Table, Debug)]
#[table(name = "posts")]
#[allow(dead_code)]
struct Post {
    #[column(pk)]
    id: Uuid,
    #[column(references = User::id, on_delete = "cascade")]
    author_id: Uuid,
    title: String,
    views: i32,
}

/// Boot embedded Postgres, apply migrations, return a connected [`Db`].
async fn setup() -> (postgresql_embedded::PostgreSQL, Db) {
    let mut postgres = postgresql_embedded::PostgreSQL::default();
    postgres.setup().await.expect("setup embedded postgres");
    postgres.start().await.expect("start embedded postgres");
    postgres
        .create_database("stakit_test")
        .await
        .expect("create database");
    let url = postgres.settings().url("stakit_test");
    let db = Db::connect(&url).await.expect("connect");
    MIGRATOR
        .run(db.pool().expect("postgres pool"))
        .await
        .expect("run migrations");
    (postgres, db)
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn end_to_end_against_real_postgres() {
    let (postgres, db) = setup().await;

    // Seed two users via the typed insert_many builder.
    let alice = Uuid::new_v4();
    let bob = Uuid::new_v4();
    db.insert_many(vec![
        UserNew {
            id: alice,
            email: "alice@x.com".to_owned(),
            name: "Alice".to_owned(),
            active: true,
        },
        UserNew {
            id: bob,
            email: "bob@x.com".to_owned(),
            name: "Bob".to_owned(),
            active: true,
        },
    ])
    .exec()
    .await
    .expect("seed users");

    // select one -> whole row, inferred type.
    let fetched = db
        .select(User::all())
        .from::<User>()
        .filter(eq(User::id, alice))
        .one()
        .await
        .expect("select one")
        .expect("row present");
    assert_eq!(fetched.email, "alice@x.com");
    assert_eq!(fetched.name, "Alice");
    // Column added by the 0002 ALTER TABLE migration, defaulted to true.
    assert!(fetched.active, "ALTER TABLE default column should be true");

    // select many ordered.
    let all = db
        .select(User::all())
        .from::<User>()
        .order_by(asc(User::email))
        .all()
        .await
        .unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].name, "Alice");
    assert_eq!(all[1].name, "Bob");

    // partial projection -> tuple.
    let emails = db
        .select((User::id, User::email))
        .from::<User>()
        .filter(eq(User::id, bob))
        .all()
        .await
        .unwrap();
    assert_eq!(emails, vec![(bob, "bob@x.com".to_owned())]);

    // any_of array bind.
    let some = db
        .select(User::all())
        .from::<User>()
        .filter(any_of(User::id, &[alice, bob]))
        .all()
        .await
        .unwrap();
    assert_eq!(some.len(), 2);

    // update.
    let affected = db
        .update::<User>()
        .set(User::name, "Alice II")
        .filter(eq(User::id, alice))
        .exec()
        .await
        .unwrap();
    assert_eq!(affected, 1);
    let renamed = db
        .select(User::all())
        .from::<User>()
        .filter(eq(User::id, alice))
        .one()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(renamed.name, "Alice II");

    // delete.
    let deleted = db
        .delete::<User>()
        .filter(eq(User::id, bob))
        .exec()
        .await
        .unwrap();
    assert_eq!(deleted, 1);
    let remaining = db.select(User::all()).from::<User>().all().await.unwrap();
    assert_eq!(remaining.len(), 1);

    // typed error mapping: duplicate unique email via the insert builder.
    let mapped = db
        .insert(UserNew {
            id: Uuid::new_v4(),
            email: "alice@x.com".to_owned(),
            name: "Dup".to_owned(),
            active: true,
        })
        .exec()
        .await
        .expect_err("expected unique violation");
    assert!(
        mapped.is_unique(),
        "expected unique violation, got {mapped:?}"
    );

    // Typed INSERT builder + RETURNING (all columns required here; none defaulted).
    let erin_id = Uuid::new_v4();
    let returned: Uuid = db
        .insert(UserNew {
            id: erin_id,
            email: "erin@x.com".to_owned(),
            name: "Erin".to_owned(),
            active: true,
        })
        .returning(User::id)
        .one()
        .await
        .expect("insert returning");
    assert_eq!(returned, erin_id);

    // insert_many in one statement.
    let inserted = db
        .insert_many(vec![
            UserNew {
                id: Uuid::new_v4(),
                email: "f@x.com".to_owned(),
                name: "F".to_owned(),
                active: true,
            },
            UserNew {
                id: Uuid::new_v4(),
                email: "g@x.com".to_owned(),
                name: "G".to_owned(),
                active: false,
            },
        ])
        .exec()
        .await
        .expect("insert_many");
    assert_eq!(inserted, 2);

    // Whole-row join tuple: (Post, Option<User>) decoded positionally.
    let post_id = Uuid::new_v4();
    db.insert(PostNew {
        id: post_id,
        author_id: erin_id,
        title: "Hello".to_owned(),
        views: 7,
    })
    .exec()
    .await
    .expect("insert post");
    let (post, author): (Post, Option<User>) = db
        .select((Post::all(), User::all().nullable()))
        .from::<Post>()
        .left_join::<User>(eq(Post::author_id, User::id))
        .filter(eq(Post::id, post_id))
        .one()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(post.title, "Hello");
    assert_eq!(post.views, 7);
    assert_eq!(author.unwrap().email, "erin@x.com");

    // Aggregate: total via count().
    let total = db.select(User::all()).from::<User>().count().await.unwrap();
    assert!(total >= 1, "expected at least one user, got {total}");

    // min/max over a column.
    let max_email: Option<String> = db
        .select(stakit_orm::max(User::email))
        .from::<User>()
        .one()
        .await
        .unwrap()
        .flatten();
    assert!(max_email.is_some());

    // Stream all rows (lazy) and count them.
    let stream = db.select(User::all()).from::<User>().stream();
    futures::pin_mut!(stream);
    let mut streamed = 0_usize;
    while let Some(row) = stream.next().await {
        row.unwrap();
        streamed += 1;
    }
    assert_eq!(streamed, usize::try_from(total).unwrap());

    // Transaction: commit path.
    let carol = Uuid::new_v4();
    db.transaction(|tx| async move {
        tx.insert(UserNew {
            id: carol,
            email: "carol@x.com".to_owned(),
            name: "Carol".to_owned(),
            active: true,
        })
        .exec()
        .await?;
        tx.update::<User>()
            .set(User::name, "Carol C")
            .filter(eq(User::id, carol))
            .exec()
            .await?;
        Ok::<_, stakit_orm::Error>(())
    })
    .await
    .expect("transaction commit");
    let carol_row = db
        .select(User::all())
        .from::<User>()
        .filter(eq(User::id, carol))
        .one()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(carol_row.name, "Carol C");

    // Transaction: rollback path (closure returns Err -> no write persists).
    let dave = Uuid::new_v4();
    let result: stakit_orm::Result<()> = db
        .transaction(|tx| async move {
            tx.insert(UserNew {
                id: dave,
                email: "dave@x.com".to_owned(),
                name: "Dave".to_owned(),
                active: true,
            })
            .exec()
            .await?;
            Err(stakit_orm::Error::Transaction("intentional rollback"))
        })
        .await;
    assert!(result.is_err());
    let dave_missing = db
        .select(User::all())
        .from::<User>()
        .filter(eq(User::id, dave))
        .one()
        .await
        .unwrap();
    assert!(
        dave_missing.is_none(),
        "rolled-back insert must not persist"
    );

    // Relations on Postgres: has_many (user -> posts) and belongs_to (post -> user),
    // each one batched IN query. `erin` authored one post (`post_id`) above.
    let users = db.select(User::all()).from::<User>().all().await.unwrap();
    let with_posts = db
        .load_has_many::<User, Post, Uuid>(users, Post::author_id, |u| u.id, |p| p.author_id)
        .await
        .expect("has_many");
    let erin_posts = with_posts
        .iter()
        .find(|(u, _)| u.id == erin_id)
        .map(|(_, posts)| posts.len())
        .expect("erin present");
    assert_eq!(erin_posts, 1, "erin should have exactly one post");

    let posts = db.select(Post::all()).from::<Post>().all().await.unwrap();
    let with_author = db
        .load_belongs_to::<Post, User, Uuid>(posts, |p| p.author_id, User::id, |u| u.id)
        .await
        .expect("belongs_to");
    let (_, author) = with_author
        .iter()
        .find(|(p, _)| p.id == post_id)
        .expect("post present");
    assert_eq!(
        author.as_ref().expect("author resolved").id,
        erin_id,
        "post must belong to erin"
    );

    postgres.stop().await.ok();
}

#[derive(DbEnum, Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    A,
    B,
}

#[derive(DbEnum, Debug, Clone, Copy, PartialEq, Eq)]
#[db_enum(int)]
enum Rank {
    Low = 1,
    High = 9,
}

#[derive(Table, Debug)]
#[table(name = "pg_things")]
#[allow(dead_code)]
struct PgThing {
    #[column(pk)]
    id: i64,
    #[column(sql_type = "text")]
    kind: Kind,
    #[column(sql_type = "int")]
    rank: Rank,
    at: chrono::DateTime<chrono::Utc>,
    local: chrono::NaiveDateTime,
    day: chrono::NaiveDate,
    alarm: chrono::NaiveTime,
    meta: serde_json::Value,
}

/// Native Postgres temporal types (timestamptz/timestamp/date/time), jsonb, and
/// `#[derive(DbEnum)]` (text + int) — round-tripped against real Postgres.
#[tokio::test]
async fn postgres_enums_temporal_json_native() {
    use chrono::{NaiveDate, NaiveTime, TimeZone, Utc};
    let (postgres, db) = setup().await;

    db.raw(
        "create table pg_things (id bigint primary key, kind text not null, rank int not null, \
         at timestamptz not null, local timestamp not null, day date not null, \
         alarm time not null, meta jsonb not null)",
    )
    .exec()
    .await
    .expect("create pg_things");

    let at = Utc.with_ymd_and_hms(2026, 6, 2, 8, 30, 0).unwrap();
    let day = NaiveDate::from_ymd_opt(1990, 1, 15).unwrap();
    let alarm = NaiveTime::from_hms_opt(8, 0, 0).unwrap();
    let local = day.and_hms_opt(8, 30, 0).unwrap();
    let meta = serde_json::json!({ "k": [1, 2, 3], "ok": true });

    db.insert(PgThingNew {
        id: 1,
        kind: Kind::B,
        rank: Rank::High,
        at,
        local,
        day,
        alarm,
        meta: meta.clone(),
    })
    .exec()
    .await
    .expect("insert pg_thing");

    let got = db.get::<PgThing>(1).one().await.unwrap().unwrap();
    assert_eq!(got.kind, Kind::B);
    assert_eq!(got.rank, Rank::High);
    assert_eq!(got.at, at);
    assert_eq!(got.local, local);
    assert_eq!(got.day, day);
    assert_eq!(got.alarm, alarm);
    assert_eq!(got.meta, meta);

    // FK ON DELETE CASCADE against real Postgres (users <- posts, defined in setup).
    let owner = uuid::Uuid::new_v4();
    db.insert(UserNew {
        id: owner,
        email: "casc@x.com".to_owned(),
        name: "Casc".to_owned(),
        active: true,
    })
    .exec()
    .await
    .unwrap();
    let post = uuid::Uuid::new_v4();
    db.insert(PostNew {
        id: post,
        author_id: owner,
        title: "p".to_owned(),
        views: 0,
    })
    .exec()
    .await
    .unwrap();
    db.delete::<User>()
        .filter(eq(User::id, owner))
        .exec()
        .await
        .unwrap();
    let orphan = db
        .select(Post::all())
        .from::<Post>()
        .filter(eq(Post::id, post))
        .one()
        .await
        .unwrap();
    assert!(orphan.is_none(), "cascade should delete the author's posts");

    // Full-text search (core Postgres, no extension): to_tsvector @@ plainto_tsquery.
    db.raw("create table pg_articles (id bigint primary key, body text not null)")
        .exec()
        .await
        .expect("create pg_articles");
    db.raw("insert into pg_articles (id, body) values (1, 'fast systems programming language')")
        .exec()
        .await
        .unwrap();
    db.raw("insert into pg_articles (id, body) values (2, 'a recipe for tomato soup')")
        .exec()
        .await
        .unwrap();
    let hits = db
        .select(PgArticle::all())
        .from::<PgArticle>()
        .filter(matches(PgArticle::body, "systems"))
        .all()
        .await
        .expect("fts");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, 1);

    postgres.stop().await.ok();
}

#[derive(Table, Debug)]
#[table(name = "pg_articles")]
#[allow(dead_code)]
struct PgArticle {
    #[column(pk)]
    id: i64,
    body: String,
}
