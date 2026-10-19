//! The `authors` / `posts` blog fixture shared by the per-backend suites
//! (`SQLite`, Turso, `MySQL`): tables, seed data, and the relation checks that run
//! unchanged on every backend. Included with `#[path]`; each suite uses a subset.
#![allow(dead_code, unreachable_pub)]

use stakit_orm::prelude::*;

#[derive(Table, Debug, Clone, PartialEq, Eq)]
#[table(name = "authors")]
pub struct Author {
    #[column(pk)]
    pub id: i64,
    pub name: String,
    // nullable column, for is_null / outer-join coverage.
    #[column(nullable)]
    pub country: Option<String>,
}

#[derive(Table, Debug, PartialEq, Eq)]
#[table(name = "posts")]
pub struct Post {
    #[column(pk)]
    pub id: i64,
    #[column(references = Author::id)]
    pub author_id: i64,
    pub title: String,
    pub views: i32,
}

/// Blog DDL for the `SQLite`-family backends (`SQLite`, Turso).
pub const SQLITE_BLOG_DDL: [&str; 2] = [
    "create table authors (id integer primary key, name text not null, country text)",
    "create table posts (id integer primary key, author_id integer not null \
     references authors(id), title text not null, views integer not null)",
];

/// Run `ddl`, then seed Ann (US; posts A1 = 10 and A2 = 5 views) and Bo (no
/// country; post B1 = 7 views).
pub async fn create_and_seed(db: &Db, ddl: &[&str]) {
    for statement in ddl {
        db.raw(*statement).exec().await.expect("blog ddl");
    }
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
    let post = |id, author_id, title: &str, views| PostNew {
        id,
        author_id,
        title: title.to_owned(),
        views,
    };
    db.insert_many(vec![
        post(1, 1, "A1", 10),
        post(2, 1, "A2", 5),
        post(3, 2, "B1", 7),
    ])
    .exec()
    .await
    .expect("seed posts");
}

/// Every post with its author (inner join, whole-row tuple, positional decode),
/// in post-id order.
pub async fn posts_with_authors(db: &Db) -> Vec<(Post, Author)> {
    db.select((Post::all(), Author::all()))
        .from::<Post>()
        .inner_join::<Author>(eq(Post::author_id, Author::id))
        .order_by(asc(Post::id))
        .all()
        .await
        .expect("inner join")
}

/// Batched `has_many` / `belongs_to` loading (one `IN` query each, no N+1)
/// against the seeded blog.
pub async fn assert_has_many_and_belongs_to(db: &Db) {
    // has_many: each author with their posts.
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
