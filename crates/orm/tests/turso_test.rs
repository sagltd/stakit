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

async fn setup() -> Db {
    let db = Db::connect_turso_local(":memory:")
        .await
        .expect("open libsql memory");
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
async fn end_to_end_against_turso() {
    let db = setup().await;

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

    // insert + RETURNING (libSQL supports it).
    let created = db
        .insert(UserNew {
            id: 3,
            email: "carol@x.com".to_owned(),
            name: "Carol".to_owned(),
            age: 40,
        })
        .returning((User::id, User::name))
        .one()
        .await
        .expect("insert returning");
    assert_eq!(created, (3, "Carol".to_owned()));

    // whole-row select.
    let fetched = db
        .select(User::all())
        .from::<User>()
        .filter(eq(User::id, 1_i64))
        .one()
        .await
        .expect("select one")
        .expect("present");
    assert_eq!(fetched.email, "alice@x.com");

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
    let some = db
        .select(User::all())
        .from::<User>()
        .filter(any_of(User::id, &[1_i64, 3]))
        .all()
        .await
        .unwrap();
    assert_eq!(some.len(), 2);

    // aggregate + count.
    let count = db.select(User::all()).from::<User>().count().await.unwrap();
    assert_eq!(count, 3);
    let oldest = db
        .select(max(User::age))
        .from::<User>()
        .one()
        .await
        .unwrap();
    assert_eq!(oldest, Some(Some(40)));

    // update.
    db.update::<User>()
        .set(User::age, 31)
        .filter(eq(User::id, 1_i64))
        .exec()
        .await
        .unwrap();
    let age = db
        .select(User::age)
        .from::<User>()
        .filter(eq(User::id, 1_i64))
        .one()
        .await
        .unwrap();
    assert_eq!(age, Some(31));

    // transaction rollback.
    let result: Result<()> = db
        .transaction(|tx| async move {
            tx.insert(UserNew {
                id: 4,
                email: "dave@x.com".to_owned(),
                name: "Dave".to_owned(),
                age: 50,
            })
            .exec()
            .await?;
            Err(Error::Transaction("rollback"))
        })
        .await;
    assert!(result.is_err());
    assert_eq!(
        db.select(User::all()).from::<User>().count().await.unwrap(),
        3,
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
        2
    );
}

#[derive(Table, Debug, PartialEq, Eq)]
#[table(name = "authors")]
struct Author {
    #[column(pk)]
    id: i64,
    name: String,
}

#[derive(Table, Debug, PartialEq, Eq)]
#[table(name = "posts")]
struct Post {
    #[column(pk)]
    id: i64,
    #[column(references = Author::id)]
    author_id: i64,
    views: i32,
}

/// Joins (multi-table positional decode) + grouped aggregate on the non-sqlx
/// libSQL backend — the same builder the sqlx backends use.
#[tokio::test]
async fn turso_joins_and_grouping() {
    use futures::StreamExt as _;
    let db = Db::connect_turso_local(":memory:").await.expect("open");
    db.raw("create table authors (id integer primary key, name text not null)")
        .exec()
        .await
        .unwrap();
    db.raw(
        "create table posts (id integer primary key, author_id integer not null \
         references authors(id), views integer not null)",
    )
    .exec()
    .await
    .unwrap();
    db.insert(AuthorNew {
        id: 1,
        name: "Ann".to_owned(),
    })
    .exec()
    .await
    .unwrap();
    db.insert_many(vec![
        PostNew {
            id: 1,
            author_id: 1,
            views: 4,
        },
        PostNew {
            id: 2,
            author_id: 1,
            views: 6,
        },
    ])
    .exec()
    .await
    .unwrap();

    // inner join whole-row tuple (Post, Author) decoded positionally via libsql.
    let joined: Vec<(Post, Author)> = db
        .select((Post::all(), Author::all()))
        .from::<Post>()
        .inner_join::<Author>(eq(Post::author_id, Author::id))
        .order_by(asc(Post::id))
        .all()
        .await
        .expect("join");
    assert_eq!(joined.len(), 2);
    assert_eq!(joined[0].1.name, "Ann");

    // group_by + sum aggregate.
    let total: Option<i64> = db
        .select(stakit_orm::sum::<Option<i64>, _, _>(Post::views))
        .from::<Post>()
        .group_by(Post::author_id)
        .one()
        .await
        .expect("sum")
        .flatten();
    assert_eq!(total, Some(10));

    // streaming on libSQL.
    let stream = db.find::<Post>().stream();
    futures::pin_mut!(stream);
    let mut n = 0;
    while let Some(r) = stream.next().await {
        r.expect("row");
        n += 1;
    }
    assert_eq!(n, 2);
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

    #[derive(Table, Debug)]
    #[table(name = "items")]
    struct Item {
        #[column(pk)]
        id: i64,
        label: String,
    }
    let n = db.select(Item::all()).from::<Item>().count().await.unwrap();
    assert_eq!(n, 1);
}
