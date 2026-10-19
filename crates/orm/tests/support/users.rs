//! The `users` table, rows, and checks shared by the per-backend end-to-end
//! suites (`SQLite`, Turso, `MySQL`). Included with `#[path]`; each suite uses a
//! subset.
#![allow(dead_code, unreachable_pub)]

use stakit_orm::prelude::*;

#[derive(Table, Debug, Clone, PartialEq, Eq)]
#[table(name = "users")]
pub struct User {
    #[column(pk)]
    pub id: i64,
    #[column(unique)]
    pub email: String,
    pub name: String,
    pub age: i32,
}

/// `users` DDL for the `SQLite`-family backends (`SQLite`, Turso).
pub const SQLITE_USERS_DDL: &str = "create table users (\
    id integer primary key, \
    email text not null unique, \
    name text not null, \
    age integer not null)";

/// A user row whose email is `<lowercase name>@x.com`.
pub fn user(id: i64, name: &str, age: i32) -> UserNew {
    UserNew {
        id,
        email: format!("{}@x.com", name.to_lowercase()),
        name: name.to_owned(),
        age,
    }
}

/// Seed Alice (id 1, 30) and Bob (id 2, 25) via `insert_many`.
pub async fn seed_alice_bob(db: &Db) {
    db.insert_many(vec![user(1, "Alice", 30), user(2, "Bob", 25)])
        .exec()
        .await
        .expect("seed users");
}

/// Select Alice (id 1) as a whole row, checking her email.
pub async fn select_alice(db: &Db) -> User {
    let fetched = db
        .select(User::all())
        .from::<User>()
        .filter(eq(User::id, 1_i64))
        .one()
        .await
        .expect("select one")
        .expect("row present");
    assert_eq!(fetched.email, "alice@x.com");
    fetched
}

/// The users whose id is in `ids` (`any_of` → `IN (?, …)` off Postgres).
pub async fn users_in(db: &Db, ids: &[i64]) -> Vec<User> {
    db.select(User::all())
        .from::<User>()
        .filter(any_of(User::id, ids))
        .all()
        .await
        .unwrap()
}

/// Number of rows in `users`.
pub async fn user_count(db: &Db) -> i64 {
    db.select(User::all()).from::<User>().count().await.unwrap()
}

/// Insert `row` inside a transaction that then fails, and check the insert was
/// rolled back (`users` still has `expected` rows).
pub async fn assert_failed_transaction_rolls_back(db: &Db, row: UserNew, expected: i64) {
    let result: Result<()> = db
        .transaction(|tx| async move {
            tx.insert(row).exec().await?;
            Err(Error::Transaction("forced rollback"))
        })
        .await;
    assert!(result.is_err());
    assert_eq!(
        user_count(db).await,
        expected,
        "rolled-back insert must not persist"
    );
}

/// Delete Bob (id 2) and check exactly one row went, leaving `remaining`.
pub async fn assert_deletes_bob(db: &Db, remaining: i64) {
    let deleted = db
        .delete::<User>()
        .filter(eq(User::id, 2_i64))
        .exec()
        .await
        .unwrap();
    assert_eq!(deleted, 1);
    assert_eq!(user_count(db).await, remaining);
}

/// Set Alice's age to 31 and read it back.
pub async fn assert_updates_alice_age(db: &Db) {
    let affected = db
        .update::<User>()
        .set(User::age, 31)
        .filter(eq(User::id, 1_i64))
        .exec()
        .await
        .unwrap();
    assert_eq!(affected, 1);
    let age = db
        .select(User::age)
        .from::<User>()
        .filter(eq(User::id, 1_i64))
        .one()
        .await
        .unwrap();
    assert_eq!(age, Some(31));
}
