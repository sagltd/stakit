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

#[derive(Table, Debug, PartialEq, Eq)]
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

    #[derive(Table, Debug)]
    #[table(name = "widgets")]
    struct Widget {
        #[column(pk)]
        id: i64,
        name: String,
    }
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
