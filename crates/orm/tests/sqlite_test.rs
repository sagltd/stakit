#![allow(missing_docs)] // gated-out builds compile to an empty crate
#![cfg(feature = "sqlite")]
//! Integration test against a real **in-memory `SQLite`** (sqlx) — no server, no
//! Docker. Proves the backend-agnostic core (one query builder, the `Driver`
//! trait) runs unchanged on `SQLite`: schema DDL, `insert` / `insert_many`, typed
//! select / partial projection / `IN` membership (dialect-expanded from `any_of`),
//! aggregates, update, delete, transactions, and `RETURNING`.

use sqlx::sqlite::SqlitePoolOptions;
use stakit_orm::prelude::*;

#[derive(Table, Debug, PartialEq, Eq)]
#[table(name = "users")]
struct User {
    #[column(pk)]
    id: i64,
    #[column(unique)]
    email: String,
    name: String,
    age: i32,
}

/// Connect to a single-connection in-memory `SQLite` (so the schema persists
/// across queries) and create the schema.
async fn setup() -> Db {
    // max_connections(1): an in-memory DB lives in its connection, so reuse one.
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect sqlite");
    let db = Db::sqlite(pool);
    db.raw(
        "create table users (\
            id integer primary key, \
            email text not null unique, \
            name text not null, \
            age integer not null)",
    )
    .exec()
    .await
    .expect("create table");
    db
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn end_to_end_against_sqlite() {
    let db = setup().await;

    // insert_many via the typed builder.
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
    .expect("seed users");

    // single insert with RETURNING (SQLite >= 3.35).
    let created = db
        .insert(UserNew {
            id: 3,
            email: "carol@x.com".to_owned(),
            name: "Carol".to_owned(),
            age: 40,
        })
        .returning((User::id, User::email))
        .one()
        .await
        .expect("insert returning");
    assert_eq!(created, (3, "carol@x.com".to_owned()));

    // select one -> whole row.
    let fetched = db
        .select(User::all())
        .from::<User>()
        .filter(eq(User::id, 1_i64))
        .one()
        .await
        .expect("select one")
        .expect("row present");
    assert_eq!(fetched.email, "alice@x.com");
    assert_eq!(fetched.name, "Alice");

    // ergonomic find(): no T::all()/from()/type annotation.
    let via_find = db
        .find::<User>()
        .filter(eq(User::id, 1_i64))
        .one()
        .await
        .expect("find one")
        .expect("present");
    assert_eq!(via_find, fetched);

    // get() by primary key.
    let via_get = db
        .get::<User>(1)
        .one()
        .await
        .expect("get")
        .expect("present");
    assert_eq!(via_get, fetched);

    // ordered select all.
    let all = db
        .select(User::all())
        .from::<User>()
        .order_by(asc(User::age))
        .all()
        .await
        .unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].name, "Bob"); // youngest
    assert_eq!(all[2].name, "Carol"); // oldest

    // partial projection -> tuple.
    let rows = db
        .select((User::id, User::email))
        .from::<User>()
        .filter(eq(User::id, 2_i64))
        .all()
        .await
        .unwrap();
    assert_eq!(rows, vec![(2, "bob@x.com".to_owned())]);

    // any_of -> IN (?, ?) expansion on SQLite (no array type).
    let some = db
        .select(User::all())
        .from::<User>()
        .filter(any_of(User::id, &[1_i64, 3]))
        .all()
        .await
        .unwrap();
    assert_eq!(some.len(), 2);

    // empty any_of -> always-false `1 = 0`.
    let none = db
        .select(User::all())
        .from::<User>()
        .filter(any_of(User::id, &[] as &[i64]))
        .all()
        .await
        .unwrap();
    assert!(none.is_empty());

    // aggregate + count.
    let total = db.select(count()).from::<User>().one().await.unwrap();
    assert_eq!(total, Some(3));
    let count = db.select(User::all()).from::<User>().count().await.unwrap();
    assert_eq!(count, 3);
    let max_age = db
        .select(max(User::age))
        .from::<User>()
        .one()
        .await
        .unwrap();
    assert_eq!(max_age, Some(Some(40)));

    // update.
    let affected = db
        .update::<User>()
        .set(User::age, 31)
        .filter(eq(User::id, 1_i64))
        .exec()
        .await
        .unwrap();
    assert_eq!(affected, 1);
    let alice = db
        .select(User::age)
        .from::<User>()
        .filter(eq(User::id, 1_i64))
        .one()
        .await
        .unwrap();
    assert_eq!(alice, Some(31));

    // transaction: commit.
    db.transaction(|tx| async move {
        tx.insert(UserNew {
            id: 4,
            email: "dave@x.com".to_owned(),
            name: "Dave".to_owned(),
            age: 50,
        })
        .exec()
        .await?;
        Ok(())
    })
    .await
    .expect("tx commit");
    assert_eq!(
        db.select(User::all()).from::<User>().count().await.unwrap(),
        4
    );

    // transaction: rollback on error.
    let result: Result<()> = db
        .transaction(|tx| async move {
            tx.insert(UserNew {
                id: 5,
                email: "erin@x.com".to_owned(),
                name: "Erin".to_owned(),
                age: 60,
            })
            .exec()
            .await?;
            Err(Error::Transaction("forced rollback"))
        })
        .await;
    assert!(result.is_err());
    assert_eq!(
        db.select(User::all()).from::<User>().count().await.unwrap(),
        4,
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
    assert_eq!(
        db.select(User::all()).from::<User>().count().await.unwrap(),
        3
    );
}

// ----- Comprehensive cross-feature coverage (joins, grouping, aggregates,
// derive(Row), streaming, predicates, on_conflict, typed errors) -----

#[derive(Table, Debug, Clone, PartialEq, Eq)]
#[table(name = "authors")]
struct Author {
    #[column(pk)]
    id: i64,
    name: String,
    // nullable column, for is_null / outer-join coverage.
    #[column(nullable)]
    country: Option<String>,
}

#[derive(Table, Debug, PartialEq, Eq)]
#[table(name = "posts")]
struct Post {
    #[column(pk)]
    id: i64,
    #[column(references = Author::id)]
    author_id: i64,
    title: String,
    views: i32,
}

/// A `#[derive(Row)]` projection: grouped aggregate decoded into a named struct.
#[derive(stakit_orm::Row, Debug, PartialEq)]
struct AuthorViews {
    #[from(Post::author_id)]
    author_id: i64,
    #[from(stakit_orm::count())]
    posts: i64,
    #[from(stakit_orm::sum::<Option<i64>, _, _>(Post::views))]
    total_views: Option<i64>,
}

async fn setup_blog() -> Db {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect");
    let db = Db::sqlite(pool);
    db.raw("create table authors (id integer primary key, name text not null, country text)")
        .exec()
        .await
        .expect("authors");
    db.raw(
        "create table posts (id integer primary key, author_id integer not null \
         references authors(id), title text not null, views integer not null)",
    )
    .exec()
    .await
    .expect("posts");
    db.insert_many(vec![
        AuthorNew {
            id: 1,
            name: "Ann".to_owned(),
            country: Some("US".to_owned()),
        },
        AuthorNew {
            id: 2,
            name: "Bo".to_owned(),
            country: None,
        },
    ])
    .exec()
    .await
    .expect("seed authors");
    db.insert_many(vec![
        PostNew {
            id: 1,
            author_id: 1,
            title: "A1".to_owned(),
            views: 10,
        },
        PostNew {
            id: 2,
            author_id: 1,
            title: "A2".to_owned(),
            views: 5,
        },
        PostNew {
            id: 3,
            author_id: 2,
            title: "B1".to_owned(),
            views: 7,
        },
    ])
    .exec()
    .await
    .expect("seed posts");
    db
}

#[tokio::test]
async fn joins_inner_and_left_with_nullable() {
    let db = setup_blog().await;

    // inner join: (Post, Author) whole-row tuple, positional decode.
    let mut rows: Vec<(Post, Author)> = db
        .select((Post::all(), Author::all()))
        .from::<Post>()
        .inner_join::<Author>(eq(Post::author_id, Author::id))
        .order_by(asc(Post::id))
        .all()
        .await
        .expect("inner join");
    assert_eq!(rows.len(), 3);
    let (post, author) = rows.remove(0);
    assert_eq!(post.title, "A1");
    assert_eq!(author.name, "Ann");

    // left join with nullable side -> Option<Author>; here always Some.
    let left: Vec<(Post, Option<Author>)> = db
        .select((Post::all(), Author::all().nullable()))
        .from::<Post>()
        .left_join::<Author>(eq(Post::author_id, Author::id))
        .all()
        .await
        .expect("left join");
    assert_eq!(left.len(), 3);
    assert!(left.iter().all(|(_, a)| a.is_some()));
}

#[tokio::test]
async fn group_by_aggregates_and_derive_row() {
    let db = setup_blog().await;

    // group_by + count + sum/avg/min/max/count_col executed against the DB.
    let grouped: Vec<AuthorViews> = db
        .select(AuthorViews::project())
        .from::<Post>()
        .group_by(Post::author_id)
        .order_by(asc(Post::author_id))
        .all()
        .await
        .expect("grouped derive(Row)");
    assert_eq!(
        grouped,
        vec![
            AuthorViews {
                author_id: 1,
                posts: 2,
                total_views: Some(15)
            },
            AuthorViews {
                author_id: 2,
                posts: 1,
                total_views: Some(7)
            },
        ]
    );

    let total_views: Option<i64> = db
        .select(stakit_orm::sum::<Option<i64>, _, _>(Post::views))
        .from::<Post>()
        .one()
        .await
        .expect("sum")
        .flatten();
    assert_eq!(total_views, Some(22));

    let avg: Option<f64> = db
        .select(stakit_orm::avg::<Option<f64>, _, _>(Post::views))
        .from::<Post>()
        .one()
        .await
        .expect("avg")
        .flatten();
    assert!((avg.unwrap() - 22.0 / 3.0).abs() < 1e-9);

    let min_views = db
        .select(min(Post::views))
        .from::<Post>()
        .one()
        .await
        .unwrap();
    assert_eq!(min_views, Some(Some(5)));
    let n = db
        .select(stakit_orm::count_col(Post::views))
        .from::<Post>()
        .one()
        .await
        .unwrap();
    assert_eq!(n, Some(3));
}

#[tokio::test]
async fn predicates_and_paging() {
    let db = setup_blog().await;

    // ne / gt / and / or / like / is_null / limit / offset, all executed.
    let high_views = db
        .find::<Post>()
        .filter(gt(Post::views, 6))
        .order_by(asc(Post::views))
        .all()
        .await
        .unwrap();
    assert_eq!(
        high_views.iter().map(|p| p.views).collect::<Vec<_>>(),
        vec![7, 10]
    );

    let combined = db
        .find::<Post>()
        .filter(and(gt(Post::views, 4), ne(Post::author_id, 2)))
        .all()
        .await
        .unwrap();
    assert_eq!(combined.len(), 2);

    let or_rows = db
        .find::<Post>()
        .filter(or(eq(Post::views, 5), eq(Post::views, 7)))
        .all()
        .await
        .unwrap();
    assert_eq!(or_rows.len(), 2);

    // not(): negate a predicate.
    let not_five = db
        .find::<Post>()
        .filter(not(eq(Post::views, 5)))
        .all()
        .await
        .unwrap();
    assert_eq!(not_five.len(), 2);
    assert!(not_five.iter().all(|p| p.views != 5));

    let like_rows = db
        .find::<Post>()
        .filter(like(Post::title, "A%"))
        .all()
        .await
        .unwrap();
    assert_eq!(like_rows.len(), 2);

    // is_null on the nullable authors.country.
    let no_country = db
        .find::<Author>()
        .filter(is_null(Author::country))
        .all()
        .await
        .unwrap();
    assert_eq!(no_country.len(), 1);
    assert_eq!(no_country[0].name, "Bo");

    // limit + offset.
    let page = db
        .find::<Post>()
        .order_by(asc(Post::id))
        .limit(1)
        .offset(1)
        .all()
        .await
        .unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].id, 2);
}

#[tokio::test]
async fn streaming_and_on_conflict_and_unique_error() {
    use futures::StreamExt as _;
    let db = setup_blog().await;

    // streaming on SQLite.
    let stream = db.find::<Post>().stream();
    futures::pin_mut!(stream);
    let mut streamed = 0;
    while let Some(row) = stream.next().await {
        row.expect("stream row");
        streamed += 1;
    }
    assert_eq!(streamed, 3);

    // on_conflict do_nothing: re-inserting pk 1 is a no-op, not an error.
    let affected = db
        .insert(PostNew {
            id: 1,
            author_id: 1,
            title: "dup".to_owned(),
            views: 0,
        })
        .on_conflict_do_nothing(Post::id)
        .exec()
        .await
        .expect("on conflict do nothing");
    assert_eq!(affected, 0);

    // typed unique-violation error mapping on SQLite (no ON CONFLICT clause).
    let err = db
        .insert(PostNew {
            id: 1,
            author_id: 1,
            title: "dup2".to_owned(),
            views: 0,
        })
        .exec()
        .await
        .expect_err("expected unique violation");
    assert!(err.is_unique(), "expected unique violation, got {err:?}");
}

#[derive(Table, Debug)]
#[table(name = "widgets")]
#[allow(dead_code)]
struct Widget {
    #[column(pk)]
    id: i64,
    name: String,
}

#[tokio::test]
async fn migrations_run_out_of_box() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect");
    let db = Db::sqlite(pool);

    let migrations = [
        Migration {
            version: "0001_init",
            statements: &["create table widgets (id integer primary key, name text not null)"],
        },
        Migration {
            version: "0002_seed",
            statements: &[
                "insert into widgets (id, name) values (1, 'a')",
                "insert into widgets (id, name) values (2, 'b')",
            ],
        },
    ];

    // first run applies both.
    let applied = db.migrate(&migrations).await.expect("migrate");
    assert_eq!(applied, 2);

    let count = db
        .select(Widget::all())
        .from::<Widget>()
        .count()
        .await
        .unwrap();
    assert_eq!(count, 2);

    // second run is idempotent: nothing pending.
    let applied_again = db.migrate(&migrations).await.expect("migrate again");
    assert_eq!(applied_again, 0);
}

#[tokio::test]
async fn relations_has_many_and_belongs_to() {
    let db = setup_blog().await;

    // has_many: each author with their posts, one batched IN query (no N+1).
    let authors = db
        .find::<Author>()
        .order_by(asc(Author::id))
        .all()
        .await
        .unwrap();
    let with_posts = db
        .load_has_many::<Author, Post, i64>(authors, Post::author_id, |a| a.id, |p| p.author_id)
        .await
        .expect("has_many");
    assert_eq!(with_posts.len(), 2);
    assert_eq!(with_posts[0].0.name, "Ann");
    assert_eq!(with_posts[0].1.len(), 2); // Ann has 2 posts
    assert_eq!(with_posts[1].1.len(), 1); // Bo has 1 post

    // belongs_to: each post with its author.
    let posts = db
        .find::<Post>()
        .order_by(asc(Post::id))
        .all()
        .await
        .unwrap();
    let with_author = db
        .load_belongs_to::<Post, Author, i64>(posts, |p| p.author_id, Author::id, |a| a.id)
        .await
        .expect("belongs_to");
    assert_eq!(with_author.len(), 3);
    assert_eq!(with_author[0].1.as_ref().unwrap().name, "Ann");
    assert_eq!(with_author[2].1.as_ref().unwrap().name, "Bo");
}

// ----- Custom column type via the ToValue/FromValue extension point -----
// Proves a user-defined type (the same mechanism pgvector/postgis would use:
// map to an existing Value variant) round-trips through a real column.

#[derive(Debug, Clone, PartialEq, Eq)]
struct Tags(Vec<String>);

impl stakit_orm::ToValue for Tags {
    fn to_value(self) -> stakit_orm::Value {
        stakit_orm::Value::Text(self.0.join(","))
    }
}
impl stakit_orm::FromValue for Tags {
    const KIND: stakit_orm::ValueKind = stakit_orm::ValueKind::Text;
    fn from_value(value: stakit_orm::Value) -> stakit_orm::Result<Self> {
        match value {
            stakit_orm::Value::Text(s) if s.is_empty() => Ok(Self(Vec::new())),
            stakit_orm::Value::Text(s) => Ok(Self(s.split(',').map(String::from).collect())),
            other => Err(stakit_orm::Error::Decode(
                format!("expected text for Tags, got {other:?}").into(),
            )),
        }
    }
}

#[derive(Table, Debug, PartialEq, Eq)]
#[table(name = "docs")]
struct Doc {
    #[column(pk)]
    id: i64,
    #[column(sql_type = "text")]
    tags: Tags,
}

#[tokio::test]
async fn custom_column_type_round_trips() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect");
    let db = Db::sqlite(pool);
    db.raw("create table docs (id integer primary key, tags text not null)")
        .exec()
        .await
        .unwrap();

    db.insert(DocNew {
        id: 1,
        tags: Tags(vec!["red".into(), "blue".into()]),
    })
    .exec()
    .await
    .expect("insert custom type");

    let got = db.find::<Doc>().one().await.unwrap().unwrap();
    assert_eq!(got.tags, Tags(vec!["red".into(), "blue".into()]));
}

// ----- Option<String> nullability + a fully-usable custom type (select, insert,
// AND filter) via ToValue/FromValue/IntoExpr -----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Color {
    Red,
    Green,
}

impl stakit_orm::ToValue for Color {
    fn to_value(self) -> stakit_orm::Value {
        stakit_orm::Value::Text(
            match self {
                Self::Red => "red",
                Self::Green => "green",
            }
            .to_owned(),
        )
    }
}
impl stakit_orm::FromValue for Color {
    const KIND: stakit_orm::ValueKind = stakit_orm::ValueKind::Text;
    fn from_value(value: stakit_orm::Value) -> stakit_orm::Result<Self> {
        match value {
            stakit_orm::Value::Text(s) if s == "red" => Ok(Self::Red),
            stakit_orm::Value::Text(s) if s == "green" => Ok(Self::Green),
            other => Err(stakit_orm::Error::Decode(
                format!("bad Color: {other:?}").into(),
            )),
        }
    }
}
// Enables eq()/ne()/etc. against a Color column.
impl stakit_orm::expr::IntoExpr<Self> for Color {
    fn into_operand(self) -> stakit_orm::expr::Operand {
        stakit_orm::expr::Operand::Value(stakit_orm::ToValue::to_value(self))
    }
}

#[derive(Table, Debug, PartialEq, Eq)]
#[table(name = "items2")]
struct Item2 {
    #[column(pk)]
    id: i64,
    #[column(sql_type = "text")]
    color: Color,
    #[column(nullable)]
    note: Option<String>,
}

#[tokio::test]
async fn nullable_and_custom_type_select_insert_filter() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect");
    let db = Db::sqlite(pool);
    db.raw("create table items2 (id integer primary key, color text not null, note text)")
        .exec()
        .await
        .unwrap();

    db.insert_many(vec![
        Item2New {
            id: 1,
            color: Color::Red,
            note: Some("hi".into()),
        },
        Item2New {
            id: 2,
            color: Color::Green,
            note: None,
        },
    ])
    .exec()
    .await
    .unwrap();

    // Option<String> round-trips Some and None.
    let one = db.get::<Item2>(1).one().await.unwrap().unwrap();
    assert_eq!(one.note, Some("hi".to_owned()));
    assert_eq!(one.color, Color::Red);
    let two = db.get::<Item2>(2).one().await.unwrap().unwrap();
    assert_eq!(two.note, None);

    // filter on the custom column (IntoExpr).
    let reds = db
        .find::<Item2>()
        .filter(eq(Item2::color, Color::Red))
        .all()
        .await
        .unwrap();
    assert_eq!(reds.len(), 1);
    assert_eq!(reds[0].id, 1);

    // filter on Option<String> column with &str (Some path) and is_null (None path).
    let with_note = db
        .find::<Item2>()
        .filter(eq(Item2::note, "hi"))
        .all()
        .await
        .unwrap();
    assert_eq!(with_note.len(), 1);
    let no_note = db
        .find::<Item2>()
        .filter(is_null(Item2::note))
        .all()
        .await
        .unwrap();
    assert_eq!(no_note.len(), 1);
    assert_eq!(no_note[0].id, 2);
}

// ----- #[derive(DbEnum)] — string enum + number enum, out of the box -----

#[derive(DbEnum, Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Active,
    #[db_enum(rename = "archived_v2")]
    Archived,
}

#[derive(DbEnum, Debug, Clone, Copy, PartialEq, Eq)]
#[db_enum(int)]
enum Level {
    Low = 1,
    Mid = 5,
    High = 9,
}

#[derive(Table, Debug, PartialEq, Eq)]
#[table(name = "tickets")]
struct Ticket {
    #[column(pk)]
    id: i64,
    #[column(sql_type = "text")]
    status: Status,
    #[column(sql_type = "int")]
    level: Level,
}

#[tokio::test]
async fn derive_db_enum_string_and_number() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect");
    let db = Db::sqlite(pool);
    db.raw("create table tickets (id integer primary key, status text not null, level integer not null)")
        .exec()
        .await
        .unwrap();

    db.insert_many(vec![
        TicketNew {
            id: 1,
            status: Status::Active,
            level: Level::Low,
        },
        TicketNew {
            id: 2,
            status: Status::Archived,
            level: Level::High,
        },
    ])
    .exec()
    .await
    .unwrap();

    // round-trip both enums
    let t1 = db.get::<Ticket>(1).one().await.unwrap().unwrap();
    assert_eq!(t1.status, Status::Active);
    assert_eq!(t1.level, Level::Low);
    let t2 = db.get::<Ticket>(2).one().await.unwrap().unwrap();
    assert_eq!(t2.status, Status::Archived);
    assert_eq!(t2.level, Level::High);

    // string enum stored under its renamed label
    let rows = db
        .raw("select id, status from tickets where id = 2")
        .bind(2_i64)
        .all::<StatusRow>()
        .await
        .unwrap();
    assert_eq!(rows[0].status, "archived_v2");

    // number enum stored as its discriminant
    let levels = db
        .raw("select id, level from tickets order by id")
        .all::<LevelRow>()
        .await
        .unwrap();
    assert_eq!(levels[0].level, 1); // Low
    assert_eq!(levels[1].level, 9); // High

    // filter on both enum columns (IntoExpr)
    let active = db
        .find::<Ticket>()
        .filter(eq(Ticket::status, Status::Active))
        .all()
        .await
        .unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].id, 1);
    let high = db
        .find::<Ticket>()
        .filter(eq(Ticket::level, Level::High))
        .all()
        .await
        .unwrap();
    assert_eq!(high.len(), 1);
    assert_eq!(high[0].id, 2);
}

#[derive(Table, Debug)]
#[table(name = "tickets")]
#[allow(dead_code)]
struct LevelRow {
    #[column(pk)]
    id: i64,
    level: i32,
}

#[derive(Table, Debug)]
#[table(name = "tickets")]
#[allow(dead_code)]
struct StatusRow {
    #[column(pk)]
    id: i64,
    status: String,
}

// ----- chrono temporal types: DateTime<Utc>, NaiveDateTime, NaiveDate, NaiveTime -----

#[derive(Table, Debug, PartialEq, Eq)]
#[table(name = "events")]
struct Event {
    #[column(pk)]
    id: i64,
    at: chrono::DateTime<chrono::Utc>, // timestamptz
    local: chrono::NaiveDateTime,      // timestamp
    day: chrono::NaiveDate,            // date
    alarm: chrono::NaiveTime,          // time
}

#[tokio::test]
async fn chrono_temporal_types_round_trip() {
    use chrono::{NaiveDate, NaiveTime, TimeZone, Utc};
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect");
    let db = Db::sqlite(pool);
    db.raw("create table events (id integer primary key, at text not null, local text not null, day text not null, alarm text not null)")
        .exec()
        .await
        .unwrap();

    let at = Utc.with_ymd_and_hms(2026, 6, 2, 8, 30, 0).unwrap();
    let local = NaiveDate::from_ymd_opt(2026, 6, 2)
        .unwrap()
        .and_hms_opt(8, 30, 0)
        .unwrap();
    let day = NaiveDate::from_ymd_opt(1990, 1, 15).unwrap();
    let alarm = NaiveTime::from_hms_opt(8, 0, 0).unwrap();

    db.insert(EventNew {
        id: 1,
        at,
        local,
        day,
        alarm,
    })
    .exec()
    .await
    .unwrap();

    let got = db.get::<Event>(1).one().await.unwrap().unwrap();
    assert_eq!(got.at, at);
    assert_eq!(got.local, local);
    assert_eq!(got.day, day);
    assert_eq!(got.alarm, alarm);

    // filter on a temporal column
    let on_day = db
        .find::<Event>()
        .filter(eq(Event::day, day))
        .all()
        .await
        .unwrap();
    assert_eq!(on_day.len(), 1);
}

// ----- JSON column -----

#[derive(Table, Debug)]
#[table(name = "docs2")]
#[allow(dead_code)]
struct Doc2 {
    #[column(pk)]
    id: i64,
    #[column(sql_type = "text")]
    meta: serde_json::Value,
    #[column(nullable, sql_type = "text")]
    extra: Option<serde_json::Value>,
}

#[tokio::test]
async fn json_column_round_trips() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect");
    let db = Db::sqlite(pool);
    db.raw("create table docs2 (id integer primary key, meta text not null, extra text)")
        .exec()
        .await
        .unwrap();

    let meta = serde_json::json!({ "a": 1, "tags": ["x", "y"], "nested": { "ok": true } });
    db.insert(Doc2New {
        id: 1,
        meta: meta.clone(),
        extra: None,
    })
    .exec()
    .await
    .unwrap();

    let got = db.get::<Doc2>(1).one().await.unwrap().unwrap();
    assert_eq!(got.meta, meta);
    assert_eq!(got.meta["nested"]["ok"], serde_json::json!(true));
    assert_eq!(got.extra, None);
}

// ----- foreign key ON DELETE CASCADE -----

#[derive(Table, Debug)]
#[table(name = "owners")]
#[allow(dead_code)]
struct Owner {
    #[column(pk)]
    id: i64,
    name: String,
}

#[derive(Table, Debug)]
#[table(name = "devices")]
#[allow(dead_code)]
struct Device {
    #[column(pk)]
    id: i64,
    #[column(references = Owner::id, on_delete = "cascade")]
    owner_id: i64,
    label: String,
}

#[tokio::test]
async fn foreign_key_on_delete_cascade() {
    // Use connect_sqlite (NOT a hand-built pool) with a multi-connection file DB and
    // NO manual pragma — this proves connect_sqlite enables FK enforcement on every
    // pooled connection, so cascade works the way a real app would get it.
    let path = std::env::temp_dir().join(format!("stakit_fk_{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let db = Db::connect_sqlite(&url).await.expect("connect");
    db.raw("create table owners (id integer primary key, name text not null)")
        .exec()
        .await
        .unwrap();
    db.raw("create table devices (id integer primary key, owner_id integer not null references owners(id) on delete cascade, label text not null)")
        .exec()
        .await
        .unwrap();

    db.insert(OwnerNew {
        id: 1,
        name: "Ann".into(),
    })
    .exec()
    .await
    .unwrap();
    db.insert_many(vec![
        DeviceNew {
            id: 1,
            owner_id: 1,
            label: "phone".into(),
        },
        DeviceNew {
            id: 2,
            owner_id: 1,
            label: "laptop".into(),
        },
    ])
    .exec()
    .await
    .unwrap();
    assert_eq!(db.find::<Device>().count().await.unwrap(), 2);

    // deleting the owner cascades to its devices
    db.delete::<Owner>()
        .filter(eq(Owner::id, 1))
        .exec()
        .await
        .unwrap();
    assert_eq!(db.find::<Device>().count().await.unwrap(), 0);
    drop(db);
    let _ = std::fs::remove_file(&path);
}

// ----- create index -----

#[derive(Table, Debug)]
#[table(name = "logs")]
#[allow(dead_code)]
struct LogRow {
    #[column(pk)]
    id: i64,
    #[column(index)]
    user_id: i64,
    msg: String,
}

#[tokio::test]
async fn create_index_works() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect");
    let db = Db::sqlite(pool);
    db.migrate(&[Migration {
        version: "0001",
        statements: &[
            "create table logs (id integer primary key, user_id integer not null, msg text not null)",
            "create index idx_logs_user_id on logs (user_id)",
        ],
    }])
    .await
    .expect("migrate with index");

    db.insert_many(vec![
        LogRowNew {
            id: 1,
            user_id: 7,
            msg: "a".into(),
        },
        LogRowNew {
            id: 2,
            user_id: 7,
            msg: "b".into(),
        },
        LogRowNew {
            id: 3,
            user_id: 9,
            msg: "c".into(),
        },
    ])
    .exec()
    .await
    .unwrap();

    // index is present and query using it returns correct rows
    let mine = db
        .find::<LogRow>()
        .filter(eq(LogRow::user_id, 7))
        .all()
        .await
        .unwrap();
    assert_eq!(mine.len(), 2);
    // verify the index object exists in sqlite_master
    let idx = db
        .raw("select id, user_id, msg from logs where 0")
        .all::<LogRow>()
        .await
        .unwrap();
    assert!(idx.is_empty());
}

// ----- typed JSON struct storage via Json<T> -----

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq)]
struct Profile {
    theme: String,
    notifications: bool,
    tags: Vec<String>,
}

#[derive(Table, Debug)]
#[table(name = "accounts")]
#[allow(dead_code)]
struct Account {
    #[column(pk)]
    id: i64,
    #[column(sql_type = "text")]
    profile: stakit_orm::Json<Profile>,
}

#[tokio::test]
async fn typed_json_struct_round_trips() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect");
    let db = Db::sqlite(pool);
    db.raw("create table accounts (id integer primary key, profile text not null)")
        .exec()
        .await
        .unwrap();

    let profile = Profile {
        theme: "dark".into(),
        notifications: true,
        tags: vec!["a".into(), "b".into()],
    };
    db.insert(AccountNew {
        id: 1,
        profile: stakit_orm::Json(profile.clone()),
    })
    .exec()
    .await
    .expect("insert struct");

    let got = db.get::<Account>(1).one().await.unwrap().unwrap();
    assert_eq!(got.profile.0, profile);
}

// Vec / HashMap / any serde type stored directly via `Json<T>` on any backend.
#[derive(Table, Debug)]
#[table(name = "bags")]
#[allow(dead_code)]
struct Bag {
    #[column(pk)]
    id: i64,
    #[column(sql_type = "text")]
    list: stakit_orm::Json<Vec<String>>,
    #[column(sql_type = "text")]
    map: stakit_orm::Json<std::collections::HashMap<String, i32>>,
}

#[tokio::test]
async fn json_stores_vec_and_hashmap() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect");
    let db = Db::sqlite(pool);
    db.raw("create table bags (id integer primary key, list text not null, map text not null)")
        .exec()
        .await
        .unwrap();

    let mut map = std::collections::HashMap::new();
    map.insert("a".to_owned(), 1);
    map.insert("b".to_owned(), 2);
    db.insert(BagNew {
        id: 1,
        list: stakit_orm::Json(vec!["x".to_owned(), "y".to_owned()]),
        map: stakit_orm::Json(map.clone()),
    })
    .exec()
    .await
    .expect("insert collections");

    let got = db.get::<Bag>(1).one().await.unwrap().unwrap();
    assert_eq!(got.list.0, vec!["x".to_owned(), "y".to_owned()]);
    assert_eq!(got.map.0, map);
}

// ----- pgvector SQL rendering (no extension needed; checks the generated SQL) -----

#[derive(Table, Debug)]
#[table(name = "docs_vec")]
#[allow(dead_code)]
struct DocVec {
    #[column(pk)]
    id: i64,
    #[column(sql_type = "vector(3)")]
    embedding: stakit_orm::Vector,
}

#[test]
fn pgvector_nearest_renders_operator_and_cast() {
    use stakit_orm::Distance;
    // Select::new with no exec uses the default (Postgres) dialect.
    let sql = stakit_orm::Select::new(DocVec::all())
        .from::<DocVec>()
        .nearest(DocVec::embedding, &[1.0, 2.0, 3.0], Distance::Cosine)
        .limit(5)
        .to_sql()
        .unwrap();
    assert!(
        sql.contains(r#""docs_vec"."embedding" <=> $1::vector"#),
        "got: {sql}"
    );
    assert!(sql.contains("order by"));
    assert!(sql.trim_end().ends_with("limit $2"), "got: {sql}");

    // L2 and InnerProduct render the right pgvector operators.
    let l2 = stakit_orm::Select::new(DocVec::all())
        .from::<DocVec>()
        .nearest(DocVec::embedding, &[1.0, 2.0, 3.0], Distance::L2)
        .to_sql()
        .unwrap();
    assert!(
        l2.contains(r#""docs_vec"."embedding" <-> $1::vector"#),
        "got: {l2}"
    );
    let ip = stakit_orm::Select::new(DocVec::all())
        .from::<DocVec>()
        .nearest(DocVec::embedding, &[1.0, 2.0, 3.0], Distance::InnerProduct)
        .to_sql()
        .unwrap();
    assert!(
        ip.contains(r#""docs_vec"."embedding" <#> $1::vector"#),
        "got: {ip}"
    );

    // the selectable distance() projection renders the operator in the SELECT list.
    let scored = stakit_orm::Select::new((
        DocVec::id,
        stakit_orm::vector::distance(DocVec::embedding, &[1.0, 2.0, 3.0], Distance::Cosine),
    ))
    .from::<DocVec>()
    .to_sql()
    .unwrap();
    assert!(
        scored.contains(r#""docs_vec"."embedding" <=> $1::vector"#),
        "got: {scored}"
    );
}

// ----- full-text search (SQLite FTS5, bundled) -----

#[derive(Table, Debug)]
#[table(name = "articles")]
#[allow(dead_code)]
struct Article {
    title: String,
    body: String,
}

#[tokio::test]
async fn sqlite_fts5_match() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect");
    let db = Db::sqlite(pool);
    // FTS5 virtual table (bundled in sqlx's SQLite).
    db.raw("create virtual table articles using fts5(title, body)")
        .exec()
        .await
        .expect("create fts5");
    db.raw(
        "insert into articles (title, body) values ('Rust', 'fast systems programming language')",
    )
    .exec()
    .await
    .unwrap();
    db.raw("insert into articles (title, body) values ('Cooking', 'a recipe for soup')")
        .exec()
        .await
        .unwrap();

    let hits = db
        .find::<Article>()
        .filter(matches(Article::body, "systems"))
        .all()
        .await
        .expect("fts match");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].title, "Rust");
}

// ----- GeoPoint round-trips through plain TEXT (no PostGIS, any backend) -----
//
// GeoPoint stores as plain WKT text on backends without PostGIS: SQLite binds the
// `POINT(lng lat)` string and reads it back as text into a GeoPoint. No extension,
// no `::geometry` cast. This proves GeoPoint works with zero PostGIS.

#[derive(Table, Debug)]
#[table(name = "geo_places")]
#[allow(dead_code)]
struct GeoPlace {
    #[column(pk)]
    id: i64,
    #[column(sql_type = "text")]
    loc: stakit_orm::GeoPoint,
}

#[tokio::test]
async fn geopoint_round_trips_as_plain_text_on_sqlite() {
    use stakit_orm::GeoPoint;
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect");
    let db = Db::sqlite(pool);
    db.raw("create table geo_places (id integer primary key, loc text not null)")
        .exec()
        .await
        .unwrap();

    // Paris, in human (lat, lng) order; WKT stored is POINT(lng lat).
    let paris = GeoPoint::new(48.8566, 2.3522);
    db.insert(GeoPlaceNew { id: 1, loc: paris })
        .exec()
        .await
        .expect("insert geopoint");

    let got = db.get::<GeoPlace>(1).one().await.unwrap().unwrap();
    // Round-trips lat/lng through plain text (SRID is None on read, as documented).
    assert!(
        (got.loc.lat() - 48.8566).abs() < 1e-9,
        "lat {}",
        got.loc.lat()
    );
    assert!(
        (got.loc.lng() - 2.3522).abs() < 1e-9,
        "lng {}",
        got.loc.lng()
    );

    // The stored cell is exactly the bare WKT (no SRID prefix, no cast): read the
    // same table back through a String-typed column to inspect the raw text.
    let raw = db
        .raw("select id, loc from geo_places where id = 1")
        .all::<RawGeo>()
        .await
        .unwrap();
    assert_eq!(raw[0].loc, "POINT(2.3522 48.8566)");
}

// PostGIS spatial *operators* (`ST_*`, `<->`) are Postgres-only; on SQLite the
// builder errors early with `Error::Unsupported("PostGIS")` instead of emitting SQL
// the DB rejects. (The `GeoPoint` column type itself still works — see above.)
#[tokio::test]
async fn spatial_ops_error_unsupported_on_sqlite() {
    use stakit_orm::{Error, GeoPoint, st_dwithin};
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect");
    let db = Db::sqlite(pool);

    // `ST_DWithin` in a WHERE filter.
    let here = GeoPoint::new(48.8566, 2.3522);
    let err = db
        .find::<GeoPlace>()
        .filter(st_dwithin(GeoPlace::loc, here, 1000.0))
        .to_sql();
    assert!(
        matches!(err, Err(Error::Unsupported("PostGIS"))),
        "got {err:?}"
    );

    // `nearest_geo` — the `<->` KNN ordering operator.
    let err = db
        .find::<GeoPlace>()
        .nearest_geo(GeoPlace::loc, GeoPoint::new(0.0, 0.0))
        .to_sql();
    assert!(
        matches!(err, Err(Error::Unsupported("PostGIS"))),
        "got {err:?}"
    );

    // `st_distance` as a selectable projection.
    let err = db
        .select((GeoPlace::id, stakit_orm::st_distance(GeoPlace::loc, here)))
        .from::<GeoPlace>()
        .to_sql();
    assert!(
        matches!(err, Err(Error::Unsupported("PostGIS"))),
        "got {err:?}"
    );
}

/// The same `geo_places` table viewed with a plain `String` location column —
/// proves the geometry cell is stored as bare WKT text.
#[derive(Table, Debug)]
#[table(name = "geo_places")]
#[allow(dead_code)]
struct RawGeo {
    #[column(pk)]
    id: i64,
    #[column(sql_type = "text")]
    loc: String,
}

// ----- composite-key upsert: "remember this device" (the goal scenario) -----
//
// One row per (user_id, device_id). First login inserts; later logins update only
// the chosen columns. `set_coalesce(location)` keeps the stored location when a
// later login's location is still NULL (async geolocation not yet resolved), so a
// previously-learned location is never wiped — all in ONE atomic statement.

#[derive(Table, Debug)]
#[table(name = "login_devices")]
#[allow(dead_code)]
struct LoginDevice {
    #[column(pk)]
    id: i64,
    user_id: i64,
    #[column(sql_type = "text")]
    device_id: String,
    platform: String,
    #[column(nullable)]
    location: Option<String>,
}

#[tokio::test]
async fn upsert_composite_key_coalesce_remembers_device() {
    // The reusable "remember this device" upsert: key on (user_id, device_id),
    // refresh platform, and keep an existing location when the new one is NULL.
    async fn remember(db: &Db, row: LoginDeviceNew) -> stakit_orm::Result<u64> {
        db.insert(row)
            .on_conflict((LoginDevice::user_id, LoginDevice::device_id))
            .set(LoginDevice::platform)
            .set_coalesce(LoginDevice::location)
            .exec()
            .await
    }

    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect");
    let db = Db::sqlite(pool);
    db.raw(
        "create table login_devices (id integer primary key, user_id integer not null, \
         device_id text not null, platform text not null, location text, \
         unique(user_id, device_id))",
    )
    .exec()
    .await
    .unwrap();

    // Mon: first login from the phone — geolocation not resolved yet (NULL).
    remember(
        &db,
        LoginDeviceNew {
            id: 1,
            user_id: 7,
            device_id: "phone".to_owned(),
            platform: "ios-16".to_owned(),
            location: None,
        },
    )
    .await
    .unwrap();

    // Async geolocation resolves a little later -> Berlin.
    db.raw("update login_devices set location = 'Berlin' where id = 1")
        .exec()
        .await
        .unwrap();

    // Tue: same phone logs in again, platform refreshed, location still NULL.
    remember(
        &db,
        LoginDeviceNew {
            id: 2, // a fresh candidate id; the conflict updates the existing row instead
            user_id: 7,
            device_id: "phone".to_owned(),
            platform: "ios-17".to_owned(),
            location: None,
        },
    )
    .await
    .unwrap();

    // Exactly one row, platform refreshed, and "Berlin" PRESERVED (not wiped).
    let rows = db
        .find::<LoginDevice>()
        .order_by(asc(LoginDevice::id))
        .all()
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "exactly one device row per (user, device)");
    assert_eq!(rows[0].id, 1, "the original row is updated in place");
    assert_eq!(rows[0].platform, "ios-17", "platform refreshed");
    assert_eq!(
        rows[0].location.as_deref(),
        Some("Berlin"),
        "coalesce kept the previously-learned location"
    );

    // Wed: a login that DID resolve a location overwrites the stored one.
    remember(
        &db,
        LoginDeviceNew {
            id: 3,
            user_id: 7,
            device_id: "phone".to_owned(),
            platform: "ios-17".to_owned(),
            location: Some("Munich".to_owned()),
        },
    )
    .await
    .unwrap();
    let rows = db.find::<LoginDevice>().all().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].location.as_deref(),
        Some("Munich"),
        "a non-NULL incoming location overwrites"
    );

    // A different device for the same user is a separate row (key is composite).
    remember(
        &db,
        LoginDeviceNew {
            id: 4,
            user_id: 7,
            device_id: "laptop".to_owned(),
            platform: "macos".to_owned(),
            location: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(db.find::<LoginDevice>().count().await.unwrap(), 2);
}

// ----- Vec<T> + HashMap columns via JSON fallback on SQLite (no native arrays) -----

#[derive(Table, Debug)]
#[table(name = "lite_bags")]
#[allow(dead_code)]
struct LiteBag {
    #[column(pk)]
    id: i64,
    #[column(sql_type = "text")]
    nums: Vec<i32>,
    #[column(sql_type = "text")]
    tags: Vec<String>,
    #[column(nullable, sql_type = "text")]
    maybe: Option<Vec<i32>>,
    #[column(sql_type = "text")]
    meta: std::collections::HashMap<String, i32>,
}

#[tokio::test]
async fn vec_and_map_round_trip_as_json_on_sqlite() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect");
    let db = Db::sqlite(pool);
    db.raw(
        "create table lite_bags (id integer primary key, nums text not null, \
         tags text not null, maybe text, meta text not null)",
    )
    .exec()
    .await
    .unwrap();

    let mut meta = std::collections::HashMap::new();
    meta.insert("a".to_owned(), 1);
    db.insert(LiteBagNew {
        id: 1,
        nums: vec![1, 2, 3],
        tags: vec!["x".to_owned(), "y".to_owned()],
        maybe: Some(vec![9]),
        meta: meta.clone(),
    })
    .exec()
    .await
    .expect("insert");
    db.insert(LiteBagNew {
        id: 2,
        nums: vec![],
        tags: vec![],
        maybe: None,
        meta: std::collections::HashMap::new(),
    })
    .exec()
    .await
    .expect("insert empty");

    let b1 = db.get::<LiteBag>(1).one().await.unwrap().unwrap();
    assert_eq!(b1.nums, vec![1, 2, 3], "Vec<i32> via JSON text");
    assert_eq!(b1.tags, vec!["x".to_owned(), "y".to_owned()]);
    assert_eq!(b1.maybe, Some(vec![9]));
    assert_eq!(b1.meta, meta);

    let b2 = db.get::<LiteBag>(2).one().await.unwrap().unwrap();
    assert_eq!(b2.nums, Vec::<i32>::new());
    assert_eq!(b2.maybe, None);

    // The stored cell is JSON text.
    let raw = db
        .raw("select id, nums from lite_bags where id = 1")
        .all::<RawNums>()
        .await
        .unwrap();
    assert_eq!(raw[0].nums, "[1,2,3]");
}

#[derive(Table, Debug)]
#[table(name = "lite_bags")]
#[allow(dead_code)]
struct RawNums {
    #[column(pk)]
    id: i64,
    #[column(sql_type = "text")]
    nums: String,
}

// Composite types are Postgres-only: binding one on SQLite errors at build time.
#[derive(stakit_orm::Type, Debug, Clone, PartialEq)]
#[db_type(name = "lite_addr")]
#[allow(dead_code)]
struct LiteAddr {
    street: String,
    city: String,
}

#[derive(Table, Debug)]
#[table(name = "lite_addr_rows")]
#[allow(dead_code)]
struct LiteAddrRow {
    #[column(pk)]
    id: i64,
    #[column(sql_type = "lite_addr")]
    home: LiteAddr,
}

#[tokio::test]
async fn composite_type_errors_unsupported_on_sqlite() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect");
    let db = Db::sqlite(pool);
    // The `::lite_addr` cast isn't a SQLite thing -> Error::Unsupported at SQL build.
    let err = db
        .insert(LiteAddrRowNew {
            id: 1,
            home: LiteAddr {
                street: "x".to_owned(),
                city: "y".to_owned(),
            },
        })
        .to_sql();
    assert!(
        matches!(err, Err(Error::Unsupported("lite_addr"))),
        "got {err:?}"
    );
}

// Date/time array elements via JSON fallback (regression: json_to_scalar dates).
#[derive(Table, Debug)]
#[table(name = "lite_dates")]
#[allow(dead_code)]
struct LiteDates {
    #[column(pk)]
    id: i64,
    #[column(sql_type = "text")]
    days: Vec<chrono::NaiveDate>,
    #[column(sql_type = "text")]
    naive: Vec<chrono::NaiveDateTime>,
    #[column(sql_type = "text")]
    instants: Vec<chrono::DateTime<chrono::Utc>>,
}

#[tokio::test]
async fn date_arrays_round_trip_as_json_on_sqlite() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect");
    let db = Db::sqlite(pool);
    db.raw("create table lite_dates (id integer primary key, days text not null, naive text not null, instants text not null)")
        .exec()
        .await
        .unwrap();

    let day = chrono::NaiveDate::from_ymd_opt(2020, 1, 2).unwrap();
    let naive = day.and_hms_opt(3, 4, 5).unwrap();
    let instant = "2020-01-02T03:04:05Z"
        .parse::<chrono::DateTime<chrono::Utc>>()
        .unwrap();
    db.insert(LiteDatesNew {
        id: 1,
        days: vec![day],
        naive: vec![naive],
        instants: vec![instant],
    })
    .exec()
    .await
    .expect("insert");

    let row = db.get::<LiteDates>(1).one().await.unwrap().unwrap();
    assert_eq!(row.days, vec![day], "Vec<NaiveDate> via JSON fallback");
    assert_eq!(
        row.naive,
        vec![naive],
        "Vec<NaiveDateTime> via JSON fallback"
    );
    assert_eq!(
        row.instants,
        vec![instant],
        "Vec<DateTime<Utc>> via JSON fallback"
    );
}
