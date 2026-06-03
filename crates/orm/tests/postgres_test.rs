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

// ----- PostGIS SQL rendering (no extension needed; checks the generated SQL) -----
//
// The embedded Postgres has no PostGIS, so these are render-only tests: they
// assert the generated SQL string, the `::geometry` cast, the `ST_*` functions,
// and the `$1`/`$2` placeholders — mirroring the pgvector render test.

#[derive(Table, Debug)]
#[table(name = "places")]
#[allow(dead_code)]
struct Place {
    #[column(pk)]
    id: i64,
    #[column(sql_type = "geometry(Point,4326)", index = "gist")]
    location: stakit_orm::GeoPoint,
}

/// A `GeoPoint` literal binds with the `::geometry` cast and a `$1` placeholder,
/// and (when a SRID is set) is wrapped in `ST_SetSRID(.., srid)`.
#[test]
fn postgis_geopoint_literal_renders_geometry_cast_and_setsrid() {
    use stakit_orm::GeoPoint;
    let here = GeoPoint::with_srid(52.52, 13.405, 4326);
    let sql = stakit_orm::Select::new(Place::all())
        .from::<Place>()
        .filter(eq(Place::location, here))
        .to_sql()
        .unwrap();
    assert!(
        sql.contains(r#""places"."location" = ST_SetSRID($1::geometry, 4326)"#),
        "got: {sql}"
    );
}

/// `st_dwithin(col, geom, distance)` renders `ST_DWithin("t"."col", $1::geometry, $2)`.
#[test]
fn postgis_st_dwithin_renders_function_cast_and_placeholders() {
    use stakit_orm::{GeoPoint, st_dwithin};
    // No SRID here so the bare `$1::geometry` cast is asserted exactly.
    let center = GeoPoint::from_lng_lat(13.405, 52.52);
    let sql = stakit_orm::Select::new(Place::all())
        .from::<Place>()
        .filter(st_dwithin(Place::location, center, 1000.0))
        .to_sql()
        .unwrap();
    assert!(
        sql.contains(r#"st_dwithin("places"."location", $1::geometry, $2)"#),
        "got: {sql}"
    );
}

/// `st_intersects(col, geom)` renders `ST_Intersects("t"."col", $1::geometry)`.
#[test]
fn postgis_st_intersects_renders_function_and_cast() {
    use stakit_orm::{Geometry, st_intersects};
    let poly = Geometry::new("POLYGON((0 0,1 0,1 1,0 1,0 0))");
    let sql = stakit_orm::Select::new(Place::all())
        .from::<Place>()
        .filter(st_intersects(Place::location, poly))
        .to_sql()
        .unwrap();
    assert!(
        sql.contains(r#"st_intersects("places"."location", $1::geometry)"#),
        "got: {sql}"
    );
}

/// The selectable `st_distance(col, geom)` projection renders in the SELECT list.
#[test]
fn postgis_st_distance_projection_renders_in_select_list() {
    use stakit_orm::{GeoPoint, st_distance};
    let here = GeoPoint::from_lng_lat(13.405, 52.52);
    let sql = stakit_orm::Select::new((Place::id, st_distance(Place::location, here)))
        .from::<Place>()
        .to_sql()
        .unwrap();
    assert!(
        sql.contains(r#"st_distance("places"."location", $1::geometry)"#),
        "got: {sql}"
    );
}

/// KNN ordering via `nearest_geo` renders `"t"."col" <-> $1::geometry`.
#[test]
fn postgis_nearest_geo_renders_knn_operator_and_cast() {
    use stakit_orm::GeoPoint;
    let here = GeoPoint::from_lng_lat(13.405, 52.52);
    let sql = stakit_orm::Select::new(Place::all())
        .from::<Place>()
        .nearest_geo(Place::location, here)
        .limit(5)
        .to_sql()
        .unwrap();
    assert!(
        sql.contains(r#""places"."location" <-> $1::geometry"#),
        "got: {sql}"
    );
    assert!(sql.contains("order by"), "got: {sql}");
    assert!(sql.trim_end().ends_with("limit $2"), "got: {sql}");
}

/// The `#[column(index = "gist")]` flows to a `CREATE INDEX ... USING gist` DDL.
#[test]
fn postgis_gist_index_ddl_renders_using_gist() {
    let location = <Place as stakit_orm::Table>::COLUMNS
        .iter()
        .find(|c| c.name == "location")
        .expect("location column");
    assert!(location.is_index);
    assert_eq!(location.index_method, Some("gist"));
    assert_eq!(
        location.create_index_sql("places").as_deref(),
        Some(r#"create index "idx_places_location" on "places" using gist ("location")"#),
    );
    // The arbitrary sql_type string flows through to the column metadata for DDL.
    assert_eq!(location.sql_type, "geometry(Point,4326)");
}

// ----- composite-key upsert against real Postgres (the goal scenario) -----
//
// One atomic `INSERT … ON CONFLICT (user_id, device_id) DO UPDATE SET …` keeps one
// row per device, refreshes chosen columns, and `set_coalesce(location)` preserves a
// previously-learned location when a later login's location is still NULL.

#[derive(Table, Debug)]
#[table(name = "pg_login_devices")]
#[allow(dead_code)]
struct PgLoginDevice {
    #[column(pk)]
    id: i64,
    user_id: i64,
    device_id: String,
    platform: String,
    #[column(nullable)]
    location: Option<String>,
}

#[tokio::test]
async fn upsert_composite_key_coalesce_remembers_device_on_postgres() {
    async fn remember(db: &Db, row: PgLoginDeviceNew) -> stakit_orm::Result<u64> {
        db.insert(row)
            .on_conflict((PgLoginDevice::user_id, PgLoginDevice::device_id))
            .set(PgLoginDevice::platform)
            .set_coalesce(PgLoginDevice::location)
            .exec()
            .await
    }

    let (postgres, db) = setup().await;
    db.raw(
        "create table pg_login_devices (id bigint primary key, user_id bigint not null, \
         device_id text not null, platform text not null, location text, \
         unique(user_id, device_id))",
    )
    .exec()
    .await
    .expect("create pg_login_devices");

    // Mon: first login, location NULL.
    remember(
        &db,
        PgLoginDeviceNew {
            id: 1,
            user_id: 7,
            device_id: "phone".to_owned(),
            platform: "ios-16".to_owned(),
            location: None,
        },
    )
    .await
    .unwrap();
    db.raw("update pg_login_devices set location = 'Berlin' where id = 1")
        .exec()
        .await
        .unwrap();

    // Tue: same device, platform refreshed, location still NULL -> Berlin preserved.
    remember(
        &db,
        PgLoginDeviceNew {
            id: 2,
            user_id: 7,
            device_id: "phone".to_owned(),
            platform: "ios-17".to_owned(),
            location: None,
        },
    )
    .await
    .unwrap();

    let rows = db
        .find::<PgLoginDevice>()
        .order_by(asc(PgLoginDevice::id))
        .all()
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "one row per (user, device)");
    assert_eq!(rows[0].id, 1, "existing row updated in place");
    assert_eq!(rows[0].platform, "ios-17", "platform refreshed");
    assert_eq!(
        rows[0].location.as_deref(),
        Some("Berlin"),
        "coalesce kept the learned location"
    );

    // Wed: a resolved location overwrites.
    remember(
        &db,
        PgLoginDeviceNew {
            id: 3,
            user_id: 7,
            device_id: "phone".to_owned(),
            platform: "ios-17".to_owned(),
            location: Some("Munich".to_owned()),
        },
    )
    .await
    .unwrap();
    let one = db
        .find::<PgLoginDevice>()
        .filter(eq(PgLoginDevice::id, 1_i64))
        .one()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(one.location.as_deref(), Some("Munich"));
    assert_eq!(db.find::<PgLoginDevice>().count().await.unwrap(), 1);

    // on_conflict + RETURNING: an upsert that conflicts updates in place and returns
    // the surviving row's id (proves the conflict clause precedes RETURNING).
    let returned: i64 = db
        .insert(PgLoginDeviceNew {
            id: 99,
            user_id: 7,
            device_id: "phone".to_owned(),
            platform: "ios-18".to_owned(),
            location: None,
        })
        .on_conflict((PgLoginDevice::user_id, PgLoginDevice::device_id))
        .set(PgLoginDevice::platform)
        .returning(PgLoginDevice::id)
        .one()
        .await
        .unwrap();
    assert_eq!(returned, 1, "RETURNING yields the existing row id on conflict-update");
    assert_eq!(db.find::<PgLoginDevice>().count().await.unwrap(), 1);

    postgres.stop().await.ok();
}
