#![allow(missing_docs)] // gated-out builds compile to an empty crate
#![cfg(feature = "postgres")]
//! End-to-end row-level-security test against a **real, embedded** Postgres.
//!
//! The DDL applied here is exactly what `stakit-orm-cli` generates for a table
//! declared with `#[table(name = "posts", rls, grant(app_user(...)),
//! policy(...))]` plus a `#[derive(Role)]` role — see the unit tests in
//! `crates/orm-cli/src/diff.rs` (which lock the emitted SQL string for string) and
//! `create_object_sql`. This test proves that generated SQL is **accepted by
//! Postgres and actually enforces the policy**: a non-owner role only sees and
//! writes rows its `USING`/`WITH CHECK` predicate permits.
//!
//! `postgresql_embedded` downloads a real Postgres binary on first run (cached
//! afterward), so this test needs network access the first time.

use sqlx::Row as _;

/// Boot embedded Postgres and return a superuser-connected pool.
async fn setup() -> (postgresql_embedded::PostgreSQL, sqlx::PgPool) {
    let mut postgres = postgresql_embedded::PostgreSQL::default();
    postgres.setup().await.expect("setup embedded postgres");
    postgres.start().await.expect("start embedded postgres");
    postgres
        .create_database("rls_test")
        .await
        .expect("create database");
    let url = postgres.settings().url("rls_test");
    let pool = sqlx::PgPool::connect(&url).await.expect("connect");
    (postgres, pool)
}

/// The forward (up) migration `stakit-orm-cli` generates for the RLS schema below.
/// Roles first, then the table object (table → grant → enable rls → policies), each a
/// separate statement — mirrors `diff::create_role_sql` + `diff::create_object_sql`.
const UP: &[&str] = &[
    r#"create role "app_user" login;"#,
    "create table \"posts\" (\n    \"id\" bigint not null,\n    \"author_id\" text not null,\n    \"title\" text not null,\n    primary key (\"id\")\n);",
    r#"grant select, insert on "posts" to "app_user";"#,
    r#"alter table "posts" enable row level security;"#,
    r#"create policy "posts_select_own" on "posts" for select to "app_user" using (author_id = current_setting('app.user_id'));"#,
    r#"create policy "posts_insert_own" on "posts" for insert to "app_user" with check (author_id = current_setting('app.user_id'));"#,
];

/// The reverse (down) migration — the exact inverse `diff::down_sql` produces.
const DOWN: &[&str] = &[
    r#"drop policy "posts_insert_own" on "posts";"#,
    r#"drop policy "posts_select_own" on "posts";"#,
    r#"alter table "posts" disable row level security;"#,
    r#"revoke select, insert on "posts" from "app_user";"#,
    r#"drop table "posts";"#,
    r#"drop role "app_user";"#,
];

async fn run(pool: &sqlx::PgPool, statements: &'static [&'static str]) {
    for statement in statements {
        sqlx::query(*statement)
            .execute(pool)
            .await
            .unwrap_or_else(|error| panic!("apply `{statement}`: {error}"));
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn generated_rls_migration_enforces_the_policy() {
    let (postgres, pool) = setup().await;

    // Apply the generated up migration.
    run(&pool, UP).await;

    // Seed as the table owner (RLS is enabled but NOT forced, so the owner bypasses
    // it for seeding) — two authors, three rows.
    for (id, author, title) in [(1_i64, "alice", "a1"), (2, "bob", "b1"), (3, "alice", "a2")] {
        sqlx::query("insert into posts (id, author_id, title) values ($1, $2, $3)")
            .bind(id)
            .bind(author)
            .bind(title)
            .execute(&pool)
            .await
            .expect("seed row");
    }

    // Everything below runs as `app_user` on ONE connection, so `set role` and the
    // session GUC persist across queries. (Pool queries can land on different
    // connections; RLS context is connection-local, so we hold a single connection.)
    let mut conn = pool.acquire().await.expect("acquire connection");

    sqlx::query("set role \"app_user\"")
        .execute(&mut *conn)
        .await
        .expect("set role app_user");

    // No GUC set yet → current_setting('app.user_id') errors, so the policy can't pass:
    // app_user sees nothing (default-deny).
    sqlx::query("select set_config('app.user_id', '', false)")
        .execute(&mut *conn)
        .await
        .expect("clear app.user_id");
    let none: Vec<i64> = sqlx::query("select id from posts order by id")
        .fetch_all(&mut *conn)
        .await
        .expect("select with empty user")
        .iter()
        .map(|row| row.get::<i64, _>("id"))
        .collect();
    assert!(
        none.is_empty(),
        "no rows should be visible when app.user_id matches nothing, got {none:?}"
    );

    // As alice → only alice's rows (1, 3).
    set_user(&mut conn, "alice").await;
    assert_eq!(
        visible_ids(&mut conn).await,
        vec![1, 3],
        "alice sees her rows"
    );

    // As bob → only bob's row (2).
    set_user(&mut conn, "bob").await;
    assert_eq!(visible_ids(&mut conn).await, vec![2], "bob sees his row");

    // INSERT WITH CHECK: as alice, inserting alice's row is allowed…
    set_user(&mut conn, "alice").await;
    sqlx::query("insert into posts (id, author_id, title) values (10, 'alice', 'a3')")
        .execute(&mut *conn)
        .await
        .expect("alice may insert her own row");

    // …but inserting a row owned by bob violates the WITH CHECK and is rejected.
    let denied = sqlx::query("insert into posts (id, author_id, title) values (11, 'bob', 'nope')")
        .execute(&mut *conn)
        .await;
    assert!(
        denied.is_err(),
        "WITH CHECK must reject an insert whose author_id != current user"
    );

    // Return the connection to a clean state before dropping it back into the pool.
    sqlx::query("reset role")
        .execute(&mut *conn)
        .await
        .expect("reset role");
    drop(conn);

    // The generated down migration reverts cleanly (drop policies → disable rls →
    // revoke → drop table → drop role), proving up/down are real inverses on pg.
    run(&pool, DOWN).await;

    postgres.stop().await.ok();
}

/// Set the per-connection RLS context variable the policy reads.
async fn set_user(conn: &mut sqlx::pool::PoolConnection<sqlx::Postgres>, user: &str) {
    sqlx::query("select set_config('app.user_id', $1, false)")
        .bind(user)
        .execute(&mut **conn)
        .await
        .expect("set app.user_id");
}

/// The ids visible to the current RLS context, ascending.
async fn visible_ids(conn: &mut sqlx::pool::PoolConnection<sqlx::Postgres>) -> Vec<i64> {
    sqlx::query("select id from posts order by id")
        .fetch_all(&mut **conn)
        .await
        .expect("select visible rows")
        .iter()
        .map(|row| row.get::<i64, _>("id"))
        .collect()
}
