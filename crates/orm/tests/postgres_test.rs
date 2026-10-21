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

#[path = "support/uuid_posts.rs"]
mod uuid_posts;
use uuid_posts::{Post, PostNew};

#[path = "support/embedded_pg.rs"]
mod embedded_pg;

/// Boot embedded Postgres, apply migrations, return a connected [`Db`].
async fn setup() -> (postgresql_embedded::PostgreSQL, Db) {
    let (postgres, url) = embedded_pg::boot("stakit_test").await;
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

#[path = "support/things.rs"]
mod things;

/// Native Postgres temporal types (timestamptz/timestamp/date/time), jsonb, and
/// `#[derive(DbEnum)]` (text + int) — round-tripped against real Postgres.
#[tokio::test]
async fn postgres_enums_temporal_json_native() {
    let (postgres, db) = setup().await;

    db.raw(
        "create table things (id bigint primary key, kind text not null, rank int not null, \
         at timestamptz not null, local timestamp not null, day date not null, \
         alarm time not null, meta jsonb not null)",
    )
    .exec()
    .await
    .expect("create things");
    things::assert_round_trips(&db).await;

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

// ----- stored-tsvector FTS against real Postgres (GIN-backed, no recompute) -----

#[derive(Table, Debug)]
#[table(name = "pg_fts_articles")]
#[allow(dead_code)]
struct PgFtsArticle {
    #[column(pk)]
    id: i64,
    body: String,
    #[column(
        sql_type = "tsvector",
        generated = "to_tsvector('english', body)",
        index = "gin"
    )]
    body_tsv: String,
}

#[tokio::test]
async fn stored_tsvector_search_matches_on_postgres() {
    let (postgres, db) = setup().await;
    // The shape the CLI now emits: a generated stored tsvector column + a GIN index.
    db.raw(
        "create table pg_fts_articles (id bigint primary key, body text not null, \
         body_tsv tsvector generated always as (to_tsvector('english', body)) stored)",
    )
    .exec()
    .await
    .expect("create pg_fts_articles");
    db.raw("create index idx_pg_fts_articles_body_tsv on pg_fts_articles using gin (body_tsv)")
        .exec()
        .await
        .expect("create gin index");

    // `body_tsv` is generated, so it is absent from the insert companion.
    db.insert(PgFtsArticleNew {
        id: 1,
        body: "fast systems programming language".to_owned(),
    })
    .exec()
    .await
    .unwrap();
    db.insert(PgFtsArticleNew {
        id: 2,
        body: "a recipe for tomato soup".to_owned(),
    })
    .exec()
    .await
    .unwrap();

    // Match the stored tsvector directly — no query-time `to_tsvector(..)` recompute.
    let hits: Vec<i64> = db
        .select(PgFtsArticle::id)
        .from::<PgFtsArticle>()
        .filter(matches_tsv(PgFtsArticle::body_tsv, "systems"))
        .all()
        .await
        .expect("stored tsv search");
    assert_eq!(hits, vec![1]);

    // The config-explicit variant matches the other row.
    let soup: Vec<i64> = db
        .select(PgFtsArticle::id)
        .from::<PgFtsArticle>()
        .filter(matches_tsv_in(PgFtsArticle::body_tsv, "soup", "english"))
        .all()
        .await
        .expect("stored tsv search (config)");
    assert_eq!(soup, vec![2]);

    postgres.stop().await.ok();
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

#[path = "support/devices.rs"]
mod devices;
use devices::LoginDevice;

#[tokio::test]
async fn upsert_composite_key_coalesce_remembers_device_on_postgres() {
    let (postgres, db) = setup().await;
    db.raw(
        "create table login_devices (id bigint primary key, user_id bigint not null, \
         device_id text not null, platform text not null, location text, \
         unique(user_id, device_id))",
    )
    .exec()
    .await
    .expect("create login_devices");
    devices::assert_remembers_device(&db).await;

    // on_conflict + RETURNING: an upsert that conflicts updates in place and returns
    // the surviving row's id (proves the conflict clause precedes RETURNING).
    let returned: i64 = db
        .insert(devices::login(99, "phone", "ios-18", None))
        .on_conflict((LoginDevice::user_id, LoginDevice::device_id))
        .set(LoginDevice::platform)
        .returning(LoginDevice::id)
        .one()
        .await
        .unwrap();
    assert_eq!(
        returned, 1,
        "RETURNING yields the existing row id on conflict-update"
    );
    assert_eq!(db.find::<LoginDevice>().count().await.unwrap(), 1);

    postgres.stop().await.ok();
}

// ----- composite PRIMARY KEY table against real Postgres (the goal_step shape) -----

#[derive(Table, Debug, PartialEq, Eq)]
#[table(name = "pg_goal_step")]
#[allow(dead_code)]
struct PgGoalStep {
    #[column(pk)]
    goal_id: i64,
    #[column(pk)]
    step_id: i64,
    position: i32,
}

#[tokio::test]
async fn composite_primary_key_table_round_trips_on_postgres() {
    let (postgres, db) = setup().await;
    // The DDL shape the CLI now emits for a two-`#[column(pk)]` struct.
    db.raw(
        "create table pg_goal_step (goal_id bigint not null, step_id bigint not null, \
         position int not null, primary key (goal_id, step_id))",
    )
    .exec()
    .await
    .expect("create pg_goal_step");

    for (goal_id, step_id, position) in [(1_i64, 1_i64, 10_i32), (1, 2, 20), (2, 1, 30)] {
        db.insert(PgGoalStepNew {
            goal_id,
            step_id,
            position,
        })
        .exec()
        .await
        .unwrap();
    }

    // The same composite key violates the primary key.
    let duplicate = db
        .insert(PgGoalStepNew {
            goal_id: 1,
            step_id: 1,
            position: 99,
        })
        .exec()
        .await;
    assert!(
        duplicate.is_err(),
        "a duplicate (goal_id, step_id) must violate the composite primary key"
    );

    // A composite-key table is queried by `find().filter(..)` — `get()` won't compile
    // for it. Fetch one junction row by both key columns.
    let row = db
        .find::<PgGoalStep>()
        .filter(and(
            eq(PgGoalStep::goal_id, 1_i64),
            eq(PgGoalStep::step_id, 2_i64),
        ))
        .one()
        .await
        .unwrap()
        .expect("row present");
    assert_eq!(row.position, 20);
    assert_eq!(db.find::<PgGoalStep>().count().await.unwrap(), 3);

    postgres.stop().await.ok();
}

// ----- #[derive(Type)] composite types against real Postgres -----

#[derive(stakit_orm::Type, Debug, Clone, PartialEq)]
#[db_type(name = "pg_address")]
#[allow(dead_code)]
struct PgAddress {
    street: String,
    city: String,
    zip: String,
    is_primary: bool,
}

#[derive(Table, Debug)]
#[table(name = "pg_people")]
#[allow(dead_code)]
struct PgPerson {
    #[column(pk)]
    id: i64,
    name: String,
    #[column(sql_type = "pg_address")]
    home: PgAddress,
    #[column(nullable, sql_type = "pg_address")]
    work: Option<PgAddress>,
}

#[tokio::test]
async fn composite_type_round_trips_on_postgres() {
    let (postgres, db) = setup().await;
    db.raw(PgAddress::create_type_sql())
        .exec()
        .await
        .expect("create type");
    db.raw(
        "create table pg_people (id bigint primary key, name text not null, \
         home pg_address not null, work pg_address)",
    )
    .exec()
    .await
    .expect("create table");

    // `street` has a comma -> exercises composite text quoting/escaping.
    let home = PgAddress {
        street: "123 Main, Apt 4".to_owned(),
        city: "Berlin".to_owned(),
        zip: "10115".to_owned(),
        is_primary: true,
    };
    let work = PgAddress {
        street: "1 Office Rd".to_owned(),
        city: "Berlin".to_owned(),
        zip: "10117".to_owned(),
        is_primary: false,
    };
    db.insert(PgPersonNew {
        id: 1,
        name: "Ann".to_owned(),
        home: home.clone(),
        work: Some(work.clone()),
    })
    .exec()
    .await
    .expect("insert with composite");
    db.insert(PgPersonNew {
        id: 2,
        name: "Bo".to_owned(),
        home: home.clone(),
        work: None,
    })
    .exec()
    .await
    .expect("insert null work");

    // Whole-row read decodes the composite transparently (selected as `home::text`).
    let p1 = db.get::<PgPerson>(1).one().await.unwrap().unwrap();
    assert_eq!(
        p1.home, home,
        "composite round-trips incl. the comma in street"
    );
    assert_eq!(p1.work, Some(work));

    let p2 = db.get::<PgPerson>(2).one().await.unwrap().unwrap();
    assert_eq!(p2.work, None, "NULL composite reads as None");

    // Filter by composite equality (the value casts to `::pg_address` in WHERE).
    let found = db
        .find::<PgPerson>()
        .filter(eq(PgPerson::home, home.clone()))
        .all()
        .await
        .expect("composite equality filter");
    assert_eq!(found.len(), 2);

    postgres.stop().await.ok();
}

// ----- native Vec<T> arrays + HashMap columns on Postgres -----

#[derive(Table, Debug)]
#[table(name = "pg_bags")]
#[allow(dead_code)]
struct PgBag {
    #[column(pk)]
    id: i64,
    #[column(sql_type = "int[]")]
    nums: Vec<i32>,
    #[column(sql_type = "text[]")]
    tags: Vec<String>,
    #[column(nullable, sql_type = "int[]")]
    maybe: Option<Vec<i32>>,
    #[column(sql_type = "jsonb")]
    meta: std::collections::HashMap<String, i32>,
}

#[tokio::test]
async fn native_arrays_and_map_round_trip_on_postgres() {
    let (postgres, db) = setup().await;
    db.raw(
        "create table pg_bags (id bigint primary key, nums int[] not null, \
         tags text[] not null, maybe int[], meta jsonb not null)",
    )
    .exec()
    .await
    .expect("create");

    let mut meta = std::collections::HashMap::new();
    meta.insert("a".to_owned(), 1);
    meta.insert("b".to_owned(), 2);
    db.insert(PgBagNew {
        id: 1,
        nums: vec![1, 2, 3],
        tags: vec!["x".to_owned(), "y, z".to_owned()],
        maybe: Some(vec![9, 8]),
        meta: meta.clone(),
    })
    .exec()
    .await
    .expect("insert");
    db.insert(PgBagNew {
        id: 2,
        nums: vec![],
        tags: vec![],
        maybe: None,
        meta: std::collections::HashMap::new(),
    })
    .exec()
    .await
    .expect("insert empty/null");

    let b1 = db.get::<PgBag>(1).one().await.unwrap().unwrap();
    assert_eq!(b1.nums, vec![1, 2, 3], "native int[] round-trips");
    assert_eq!(b1.tags, vec!["x".to_owned(), "y, z".to_owned()], "text[]");
    assert_eq!(b1.maybe, Some(vec![9, 8]));
    assert_eq!(b1.meta, meta, "HashMap via jsonb");

    let b2 = db.get::<PgBag>(2).one().await.unwrap().unwrap();
    assert_eq!(b2.nums, Vec::<i32>::new(), "empty array");
    assert_eq!(b2.maybe, None, "NULL array reads as None");
    assert!(b2.meta.is_empty());

    postgres.stop().await.ok();
}

// ----- native date/time arrays on Postgres (regression: read/null symmetry) -----

#[derive(Table, Debug)]
#[table(name = "pg_dates")]
#[allow(dead_code)]
struct PgDates {
    #[column(pk)]
    id: i64,
    #[column(sql_type = "date[]")]
    days: Vec<chrono::NaiveDate>,
    #[column(sql_type = "timestamptz[]")]
    instants: Vec<chrono::DateTime<chrono::Utc>>,
    #[column(sql_type = "timestamp[]")]
    naive: Vec<chrono::NaiveDateTime>,
    #[column(nullable, sql_type = "timestamp[]")]
    maybe: Option<Vec<chrono::NaiveDateTime>>,
}

#[tokio::test]
async fn native_date_time_arrays_round_trip_on_postgres() {
    let (postgres, db) = setup().await;
    db.raw(
        "create table pg_dates (id bigint primary key, days date[] not null, \
         instants timestamptz[] not null, naive timestamp[] not null, maybe timestamp[])",
    )
    .exec()
    .await
    .expect("create");

    let day = chrono::NaiveDate::from_ymd_opt(2020, 1, 2).unwrap();
    let instant = "2020-01-02T03:04:05Z"
        .parse::<chrono::DateTime<chrono::Utc>>()
        .unwrap();
    let naive = day.and_hms_opt(3, 4, 5).unwrap();
    db.insert(PgDatesNew {
        id: 1,
        days: vec![day],
        instants: vec![instant],
        naive: vec![naive],
        maybe: None,
    })
    .exec()
    .await
    .expect("insert date arrays");

    let row = db.get::<PgDates>(1).one().await.unwrap().unwrap();
    assert_eq!(row.days, vec![day]);
    assert_eq!(row.instants, vec![instant]);
    assert_eq!(row.naive, vec![naive]);
    assert_eq!(row.maybe, None, "NULL timestamp[] reads as None");

    postgres.stop().await.ok();
}

#[tokio::test]
async fn guarded_upsert_locked_budget_is_unchanged() {
    let (_postgres, db) = setup().await;
    let owner = Uuid::new_v4();
    db.insert(UserNew {
        id: owner,
        email: "budget@x.test".into(),
        name: "Budget".into(),
        active: true,
    })
    .exec()
    .await
    .unwrap();
    let id = Uuid::new_v4();
    db.insert(PostNew {
        id,
        author_id: owner,
        title: "locked".into(),
        views: 3,
    })
    .exec()
    .await
    .unwrap();
    // The guard is evaluated while PostgreSQL holds the conflicting row lock.
    let affected = db
        .insert(PostNew {
            id,
            author_id: owner,
            title: "incoming".into(),
            views: 1,
        })
        .on_conflict(Post::id)
        .set_add(Post::views)
        .where_conflict(lt(stored(Post::views), 3_i32))
        .exec()
        .await
        .unwrap();
    assert_eq!(
        affected, 0,
        "a locked budget must reject the conflict update"
    );
}

#[path = "support/guarded_upsert.rs"]
mod guarded_upsert;

#[tokio::test]
async fn guarded_upsert_login_windows_and_concurrent_budget() {
    use guarded_upsert::{Attempt, attempt_row, reserve};
    let (_postgres, db) = setup_guarded_attempts().await;
    // Fresh inserts bypass a false update guard.
    assert_eq!(
        reserve(db.insert(attempt_row("first", "key")), 300, 0)
            .exec()
            .await
            .unwrap(),
        1
    );
    let first = db
        .select(Attempt::all())
        .from::<Attempt>()
        .one()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reserve(db.insert(attempt_row("second", "key")), 300, 3)
            .returning(Attempt::failed_count)
            .one()
            .await
            .unwrap(),
        2
    );
    let active = db
        .select(Attempt::all())
        .from::<Attempt>()
        .one()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(active.window_started_at, first.window_started_at);
    assert_eq!(active.id, "first");
    // A burst of concurrent conflicts consumes exactly the remaining budget.
    let results = futures::future::join_all((0..20).map(|index| {
        let db = db.clone();
        async move {
            reserve(
                db.insert(attempt_row(&format!("burst-{index}"), "key")),
                300,
                5,
            )
            .exec()
            .await
            .unwrap()
        }
    }))
    .await;
    assert_eq!(results.iter().sum::<u64>(), 3);
    assert!(
        reserve(db.insert(attempt_row("locked", "key")), 300, 5)
            .returning(Attempt::id)
            .all()
            .await
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        reserve(db.insert(attempt_row("locked-one", "key")), 300, 5)
            .returning(Attempt::id)
            .one()
            .await,
        Err(stakit_orm::Error::NotFound)
    ));
    db.update::<Attempt>()
        .set(Attempt::window_started_at, db_now_minus_seconds(600))
        .filter(eq(Attempt::identifier, "key"))
        .exec()
        .await
        .unwrap();
    let expired = db
        .select(Attempt::window_started_at)
        .from::<Attempt>()
        .one()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reserve(db.insert(attempt_row("reset", "key")), 300, 5)
            .exec()
            .await
            .unwrap(),
        1
    );
    let reset = db
        .select(Attempt::all())
        .from::<Attempt>()
        .one()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reset.failed_count, 1);
    assert!(reset.window_started_at > expired);
}

#[tokio::test]
async fn guarded_upsert_verification_resends_reset_increment_and_serialize() {
    use guarded_upsert::{Verification, resend, verification_row};
    let (_postgres, db) = setup().await;
    embedded_pg::run(db.pool().unwrap(), &[
        "create table verifications (id text primary key, user_id text not null, purpose text not null, method text not null, code text not null, target text, created_at timestamptz not null default now(), tries integer not null default 0, resend_count integer not null default 0, unique(user_id, purpose))",
    ]).await;
    resend(db.insert(verification_row("first", "old")), 300)
        .exec()
        .await
        .unwrap();
    db.update::<Verification>()
        .set(Verification::tries, 4_i32)
        .exec()
        .await
        .unwrap();
    let mut incoming = verification_row("second", "new");
    incoming.method = "phone".into();
    incoming.target = None;
    resend(db.insert(incoming), 300).exec().await.unwrap();
    let active = db
        .select(Verification::all())
        .from::<Verification>()
        .one()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            active.id.as_str(),
            active.method.as_str(),
            active.code.as_str()
        ),
        ("first", "phone", "new")
    );
    assert_eq!(
        (active.tries, active.resend_count, active.target),
        (0, 1, None)
    );
    let results = futures::future::join_all((0..12).map(|index| {
        let db = db.clone();
        async move {
            resend(
                db.insert(verification_row(&format!("resend-{index}"), "burst")),
                300,
            )
            .exec()
            .await
            .unwrap()
        }
    }))
    .await;
    assert_eq!(results.iter().sum::<u64>(), 12);
    let count = db
        .select(Verification::resend_count)
        .from::<Verification>()
        .one()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(count, 13);
    db.update::<Verification>()
        .set(Verification::created_at, db_now_minus_seconds(600))
        .exec()
        .await
        .unwrap();
    let expired = db
        .select(Verification::created_at)
        .from::<Verification>()
        .one()
        .await
        .unwrap()
        .unwrap();
    let reset = resend(db.insert(verification_row("reset", "fresh")), 300)
        .returning((Verification::resend_count, Verification::created_at))
        .one()
        .await
        .unwrap();
    assert_eq!(reset.0, 0);
    assert!(reset.1 > expired);
}

async fn setup_guarded_attempts() -> (postgresql_embedded::PostgreSQL, Db) {
    let (postgres, db) = setup().await;
    embedded_pg::run(db.pool().unwrap(), &[
        "create table login_attempts (id text primary key, identifier text unique not null, failed_count integer not null default 0, window_started_at timestamptz not null default now())",
    ]).await;
    (postgres, db)
}

#[tokio::test]
async fn guarded_upsert_login_window_exact_cutoff() {
    use guarded_upsert::{Attempt, attempt_row, reserve};
    let (_postgres, db) = setup_guarded_attempts().await;
    reserve(db.insert(attempt_row("first", "key")), 300, 5)
        .exec()
        .await
        .unwrap();
    // Exact equality at the cutoff is expired (transaction-stable database now).
    db.transaction(|tx| async move {
        tx.update::<Attempt>()
            .set(Attempt::window_started_at, db_now_minus_seconds(300))
            .set(Attempt::failed_count, 5_i32)
            .exec()
            .await
            .unwrap();
        assert_eq!(
            reserve(tx.insert(attempt_row("boundary", "key")), 300, 5)
                .returning(Attempt::failed_count)
                .one()
                .await
                .unwrap(),
            1
        );
        Ok(())
    })
    .await
    .unwrap();
}
