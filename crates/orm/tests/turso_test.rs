#![allow(missing_docs)] // gated-out builds compile to an empty crate
#![cfg(feature = "turso")]
//! Integration test against a real **in-memory Turso / `libSQL`** database — the
//! non-sqlx backend. No server, no Docker. This is the proof that the `Driver`
//! abstraction is genuinely backend-agnostic: the *exact same* query builder that
//! drives Postgres/`SQLite`/`MySQL` (all sqlx) also drives `libSQL` (not sqlx),
//! with only the `Db` constructor differing.
//!
//! Exercises schema DDL, `insert` / `insert_many`, typed select / partial projection
//! / `IN` membership, aggregates, update, delete, transactions, and `RETURNING`
//! (`libSQL` supports it).

use stakit_orm::prelude::*;

#[path = "support/users.rs"]
mod users;
use users::{User, user};

async fn setup() -> Db {
    let db = Db::connect_turso_local(":memory:")
        .await
        .expect("open libsql memory");
    db.raw(users::SQLITE_USERS_DDL)
        .exec()
        .await
        .expect("create table");
    db
}

#[tokio::test]
async fn end_to_end_against_turso() {
    let db = setup().await;
    users::seed_alice_bob(&db).await;

    // insert + RETURNING (libSQL supports it).
    let created = db
        .insert(user(3, "Carol", 40))
        .returning((User::id, User::name))
        .one()
        .await
        .expect("insert returning");
    assert_eq!(created, (3, "Carol".to_owned()));

    // whole-row select.
    users::select_alice(&db).await;

    // ordered + partial projection.
    let names = db
        .select(User::name)
        .from::<User>()
        .order_by(desc(User::age))
        .all()
        .await
        .unwrap();
    assert_eq!(names, vec!["Carol", "Alice", "Bob"]);

    // any_of -> IN (?, ?).
    assert_eq!(users::users_in(&db, &[1, 3]).await.len(), 2);

    // aggregate + count.
    assert_eq!(users::user_count(&db).await, 3);
    let oldest = db
        .select(max(User::age))
        .from::<User>()
        .one()
        .await
        .unwrap();
    assert_eq!(oldest, Some(Some(40)));

    users::assert_updates_alice_age(&db).await;
    users::assert_failed_transaction_rolls_back(&db, user(4, "Dave", 50), 3).await;
    users::assert_deletes_bob(&db, 2).await;
}

#[path = "support/blog.rs"]
mod blog;
use blog::Post;

async fn setup_blog() -> Db {
    let db = Db::connect_turso_local(":memory:").await.expect("open");
    blog::create_and_seed(&db, &blog::SQLITE_BLOG_DDL).await;
    db
}

/// Joins (multi-table positional decode) + grouped aggregate on the non-sqlx
/// libSQL backend — the same builder the sqlx backends use.
#[tokio::test]
async fn turso_joins_and_grouping() {
    use futures::StreamExt as _;
    let db = setup_blog().await;

    // inner join whole-row tuple (Post, Author) decoded positionally via libsql.
    let joined = blog::posts_with_authors(&db).await;
    assert_eq!(joined.len(), 3);
    assert_eq!(joined[0].1.name, "Ann");

    // group_by + sum aggregate (Ann's group).
    let total: Option<i64> = db
        .select(stakit_orm::sum::<Option<i64>, _, _>(Post::views))
        .from::<Post>()
        .filter(eq(Post::author_id, 1_i64))
        .group_by(Post::author_id)
        .one()
        .await
        .expect("sum")
        .flatten();
    assert_eq!(total, Some(15));

    // streaming on libSQL.
    let stream = db.find::<Post>().stream();
    futures::pin_mut!(stream);
    let mut n = 0;
    while let Some(r) = stream.next().await {
        r.expect("row");
        n += 1;
    }
    assert_eq!(n, 3);
}

/// Universal migrations run out-of-box on the non-sqlx libSQL backend too.
#[tokio::test]
async fn turso_migrations_run_out_of_box() {
    let db = Db::connect_turso_local(":memory:").await.expect("open");
    let migrations = [
        Migration {
            version: "0001",
            statements: &["create table items (id integer primary key, label text not null)"],
        },
        Migration {
            version: "0002",
            statements: &["insert into items (id, label) values (1, 'x')"],
        },
    ];
    assert_eq!(db.migrate(&migrations).await.expect("migrate"), 2);
    assert_eq!(db.migrate(&migrations).await.expect("again"), 0);

    let n = db.select(Item::all()).from::<Item>().count().await.unwrap();
    assert_eq!(n, 1);
}

#[derive(Table, Debug)]
#[table(name = "items")]
#[allow(dead_code)]
struct Item {
    #[column(pk)]
    id: i64,
    label: String,
}

/// `has_many` / `belongs_to` batched relations on the non-sqlx `libSQL` backend.
#[tokio::test]
async fn turso_relations() {
    let db = setup_blog().await;
    blog::assert_has_many_and_belongs_to(&db).await;
}

#[path = "support/things.rs"]
mod things;

/// Enums (string + int), all chrono temporal types, and JSON on the non-sqlx
/// libSQL backend (stored as TEXT/INTEGER and round-tripped).
#[tokio::test]
async fn turso_enums_temporal_json() {
    let db = Db::connect_turso_local(":memory:").await.expect("open");
    db.raw(
        "create table things (id integer primary key, kind text not null, rank integer not null, \
         at text not null, day text not null, alarm text not null, local text not null, \
         meta text not null)",
    )
    .exec()
    .await
    .unwrap();
    things::assert_round_trips(&db).await;
}

#[derive(Table, Debug)]
#[table(name = "tt_owners")]
#[allow(dead_code)]
struct TtOwner {
    #[column(pk)]
    id: i64,
    name: String,
}

#[derive(Table, Debug)]
#[table(name = "tt_devices")]
#[allow(dead_code)]
struct TtDevice {
    #[column(pk)]
    id: i64,
    #[column(references = TtOwner::id, on_delete = "cascade")]
    owner_id: i64,
}

/// FK ON DELETE CASCADE on `libSQL` — proves `connect_turso_local` enables FK
/// enforcement (off by default), so deleting an owner removes its devices.
#[tokio::test]
async fn turso_foreign_key_cascade() {
    let db = Db::connect_turso_local(":memory:").await.expect("open");
    db.raw("create table tt_owners (id integer primary key, name text not null)")
        .exec()
        .await
        .unwrap();
    db.raw("create table tt_devices (id integer primary key, owner_id integer not null references tt_owners(id) on delete cascade)")
        .exec()
        .await
        .unwrap();
    db.insert(TtOwnerNew {
        id: 1,
        name: "Ann".into(),
    })
    .exec()
    .await
    .unwrap();
    db.insert_many(vec![
        TtDeviceNew { id: 1, owner_id: 1 },
        TtDeviceNew { id: 2, owner_id: 1 },
    ])
    .exec()
    .await
    .unwrap();
    assert_eq!(db.find::<TtDevice>().count().await.unwrap(), 2);

    db.delete::<TtOwner>()
        .filter(eq(TtOwner::id, 1))
        .exec()
        .await
        .unwrap();
    assert_eq!(
        db.find::<TtDevice>().count().await.unwrap(),
        0,
        "cascade must delete devices when owner is deleted"
    );
}

#[derive(Table, Debug)]
#[table(name = "embeds")]
#[allow(dead_code)]
struct Embed {
    #[column(pk)]
    id: i64,
    title: String,
    #[column(sql_type = "blob")]
    embedding: stakit_orm::Vector,
}

/// Native Turso/libSQL vector search: insert via `vector32(..)`, nearest-neighbour
/// via `vector_distance_cos(..)` + ORDER BY + LIMIT (the `.nearest()` builder).
#[tokio::test]
async fn turso_vector_search() {
    use stakit_orm::Distance;
    let db = Db::connect_turso_local(":memory:").await.expect("open");
    db.raw("create table embeds (id integer primary key, title text not null, embedding blob not null)")
        .exec()
        .await
        .unwrap();

    db.insert_many(vec![
        EmbedNew {
            id: 1,
            title: "cat".into(),
            embedding: stakit_orm::Vector::new(vec![1.0, 0.0, 0.0]),
        },
        EmbedNew {
            id: 2,
            title: "dog".into(),
            embedding: stakit_orm::Vector::new(vec![0.9, 0.1, 0.0]),
        },
        EmbedNew {
            id: 3,
            title: "car".into(),
            embedding: stakit_orm::Vector::new(vec![0.0, 0.0, 1.0]),
        },
    ])
    .exec()
    .await
    .expect("insert embeddings");

    // nearest to [1,0,0] by cosine: cat (exact), then dog, then car.
    let query = [1.0_f32, 0.0, 0.0];
    let nearest = db
        .find::<Embed>()
        .nearest(Embed::embedding, &query, Distance::Cosine)
        .limit(2)
        .all()
        .await
        .expect("nearest");
    assert_eq!(nearest.len(), 2);
    assert_eq!(nearest[0].title, "cat");
    assert_eq!(nearest[1].title, "dog");

    // get the distance SCORE back via the selectable `distance(..)` projection.
    let scored = db
        .select((
            Embed::title,
            stakit_orm::vector::distance(Embed::embedding, &query, Distance::Cosine),
        ))
        .from::<Embed>()
        .nearest(Embed::embedding, &query, Distance::Cosine)
        .limit(1)
        .all()
        .await
        .expect("distance score");
    assert_eq!(scored[0].0, "cat");
    assert!(
        scored[0].1 < 1e-6,
        "exact match cosine distance ~0, got {}",
        scored[0].1
    );

    // L2 metric also works.
    let by_l2 = db
        .find::<Embed>()
        .nearest(Embed::embedding, &query, Distance::L2)
        .limit(1)
        .all()
        .await
        .expect("l2");
    assert_eq!(by_l2[0].title, "cat");

    // Vector round-trip read (libSQL blob -> Vec<f32>).
    let cat = db.get::<Embed>(1).one().await.unwrap().unwrap();
    assert_eq!(cat.embedding.0, vec![1.0, 0.0, 0.0]);
}

#[derive(Table, Debug)]
#[table(name = "tt_articles")]
#[allow(dead_code)]
struct TtArticle {
    title: String,
    body: String,
}

/// Full-text search on libSQL via FTS5 `MATCH` (bundled in libSQL).
#[tokio::test]
async fn turso_fts5_match() {
    let db = Db::connect_turso_local(":memory:").await.expect("open");
    db.raw("create virtual table tt_articles using fts5(title, body)")
        .exec()
        .await
        .expect("create fts5");
    db.raw("insert into tt_articles (title, body) values ('Rust', 'fast systems programming')")
        .exec()
        .await
        .unwrap();
    db.raw("insert into tt_articles (title, body) values ('Soup', 'a tasty recipe')")
        .exec()
        .await
        .unwrap();
    let hits = db
        .find::<TtArticle>()
        .filter(matches(TtArticle::body, "systems"))
        .all()
        .await
        .expect("fts match");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].title, "Rust");
}

// ----- composite-key upsert with coalesce against real Turso / libSQL -----
//
// Turso shares the standard `ON CONFLICT (...)` path with SQLite. This proves it
// end-to-end on the non-sqlx engine: one row per (user_id, device_id), and
// `set_coalesce` preserves a learned location across a later NULL-location login.

#[path = "support/devices.rs"]
mod devices;

#[tokio::test]
async fn upsert_composite_key_coalesce_remembers_device_on_turso() {
    let db = Db::connect_turso_local(":memory:").await.expect("open");
    db.raw(devices::SQLITE_DDL).exec().await.expect("create");
    devices::assert_remembers_device(&db).await;
}

// ----- Vec<T> + HashMap columns via JSON fallback on Turso/libSQL -----

#[derive(Table, Debug)]
#[table(name = "tt_bags")]
#[allow(dead_code)]
struct TtBag {
    #[column(pk)]
    id: i64,
    #[column(sql_type = "text")]
    nums: Vec<i32>,
    #[column(sql_type = "text")]
    meta: std::collections::HashMap<String, i32>,
}

#[tokio::test]
async fn vec_and_map_round_trip_as_json_on_turso() {
    let db = Db::connect_turso_local(":memory:").await.expect("open");
    db.raw("create table tt_bags (id integer primary key, nums text not null, meta text not null)")
        .exec()
        .await
        .expect("create");
    let mut meta = std::collections::HashMap::new();
    meta.insert("a".to_owned(), 1);
    db.insert(TtBagNew {
        id: 1,
        nums: vec![4, 5, 6],
        meta: meta.clone(),
    })
    .exec()
    .await
    .expect("insert");

    let bag = db.get::<TtBag>(1).one().await.unwrap().unwrap();
    assert_eq!(bag.nums, vec![4, 5, 6]);
    assert_eq!(bag.meta, meta);
}
