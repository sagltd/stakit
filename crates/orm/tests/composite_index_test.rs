#![allow(missing_docs)] // gated-out builds compile to an empty crate
#![cfg(feature = "postgres")]
//! End-to-end test for **table-level (composite / unique / GIN) indexes** against a
//! real, embedded Postgres.
//!
//! The `#[derive(Table)]` below proves the macro accepts and validates the
//! `index(...)`/`unique_index(...)` table attributes at **compile time** (each
//! indexed column must be a real field). The applied DDL is exactly what
//! `stakit-orm-cli` generates for it (locked string-for-string by the
//! `create_table_index_sql` unit tests in `crates/orm-cli/src/diff.rs`); this test
//! proves that DDL is **accepted by Postgres and the indexes actually exist** in
//! `pg_indexes`, and that the `down` migration drops them.

use sqlx::Row as _;

/// Compile-time proof that `#[derive(Table)]` accepts the index attributes and that
/// every indexed column resolves to a real field (an unknown column would not build).
#[derive(stakit_orm::Table, Debug)]
#[table(
    name = "event",
    index(idx_event_window = (account_id, user_id, session_id, seq)),
    unique_index(uq_event_session_seq = (session_id, seq)),
    index(idx_event_desc_gin = (desc_tsv), method = "gin")
)]
#[allow(dead_code)]
struct Event {
    #[column(pk)]
    id: i64,
    account_id: i64,
    user_id: i64,
    session_id: i64,
    seq: i64,
    body: String,
    // A generated tsvector; the GIN index is built on it. `body` is declared first so
    // the generation expression can reference it (the caveat from the goal).
    #[column(sql_type = "tsvector", generated = "to_tsvector('english', body)")]
    desc_tsv: String,
}

/// The forward (up) migration `stakit-orm-cli` generates for `Event`: the table, then
/// the composite / unique-composite / GIN indexes (indexes after the table, so the
/// generated `desc_tsv` column exists before the GIN index references it).
const UP: &[&str] = &[
    "create table \"event\" (\n    \"id\" bigint not null,\n    \"account_id\" bigint not null,\n    \
     \"user_id\" bigint not null,\n    \"session_id\" bigint not null,\n    \"seq\" bigint not null,\n    \
     \"body\" text not null,\n    \"desc_tsv\" tsvector generated always as (to_tsvector('english', body)) stored,\n    \
     primary key (\"id\")\n);",
    r#"create index "idx_event_window" on "event" ("account_id", "user_id", "session_id", "seq");"#,
    r#"create unique index "uq_event_session_seq" on "event" ("session_id", "seq");"#,
    r#"create index "idx_event_desc_gin" on "event" using gin ("desc_tsv");"#,
];

/// The reverse (down) migration: drop the indexes (reverse order), then the table.
const DOWN: &[&str] = &[
    r#"drop index "idx_event_desc_gin";"#,
    r#"drop index "uq_event_session_seq";"#,
    r#"drop index "idx_event_window";"#,
    r#"drop table "event";"#,
];

async fn setup() -> (postgresql_embedded::PostgreSQL, sqlx::PgPool) {
    let mut postgres = postgresql_embedded::PostgreSQL::default();
    postgres.setup().await.expect("setup embedded postgres");
    postgres.start().await.expect("start embedded postgres");
    postgres
        .create_database("index_test")
        .await
        .expect("create database");
    let url = postgres.settings().url("index_test");
    let pool = sqlx::PgPool::connect(&url).await.expect("connect");
    (postgres, pool)
}

async fn run(pool: &sqlx::PgPool, statements: &'static [&'static str]) {
    for statement in statements {
        sqlx::query(*statement)
            .execute(pool)
            .await
            .unwrap_or_else(|error| panic!("apply `{statement}`: {error}"));
    }
}

async fn index_names(pool: &sqlx::PgPool) -> Vec<String> {
    sqlx::query("select indexname from pg_indexes where tablename = 'event' order by indexname")
        .fetch_all(pool)
        .await
        .expect("query pg_indexes")
        .iter()
        .map(|row| row.get::<String, _>("indexname"))
        .collect()
}

#[tokio::test]
async fn generated_composite_indexes_exist_in_postgres_and_revert() {
    let (postgres, pool) = setup().await;

    run(&pool, UP).await;
    let names = index_names(&pool).await;
    // The three table-level indexes (plus the implicit primary-key index) are present.
    for expected in [
        "idx_event_window",
        "uq_event_session_seq",
        "idx_event_desc_gin",
    ] {
        assert!(
            names.iter().any(|name| name == expected),
            "expected index `{expected}` in pg_indexes, got {names:?}"
        );
    }

    // The unique index actually enforces uniqueness on (session_id, seq).
    sqlx::query(
        "insert into event (id, account_id, user_id, session_id, seq, body) \
         values (1, 1, 1, 7, 1, 'a')",
    )
    .execute(&pool)
    .await
    .expect("first row");
    let duplicate = sqlx::query(
        "insert into event (id, account_id, user_id, session_id, seq, body) \
         values (2, 1, 1, 7, 1, 'b')",
    )
    .execute(&pool)
    .await;
    assert!(
        duplicate.is_err(),
        "unique index must reject a duplicate (session_id, seq)"
    );

    // The down migration drops every generated index (and the table).
    run(&pool, DOWN).await;

    postgres.stop().await.ok();
}
