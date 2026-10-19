//! Raw `sqlx` vs `stakit-orm` — latency **and allocations** on in-memory `SQLite`.
//!
//! Proves the ORM adds negligible overhead over hand-written sqlx for the three
//! shapes: insert, simple point-select, medium filtered/ordered/limited select.
//! Reads run against a fixed 1000-row table; inserts use a separate table so
//! read latency stays deterministic as inserts accumulate.
//!
//! `divan::AllocProfiler` reports bytes + allocation counts per op (memory).
#![cfg(feature = "sqlite")]
#![allow(missing_docs)]

#[macro_use]
#[path = "common/db.rs"]
mod common;

db_benches! {
    pool: sqlx::SqlitePool,
    connect: async {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect");
        let db = Db::sqlite(pool.clone());
        (Box::new(()) as Box<dyn std::any::Any + Send + Sync>, pool, db)
    },
    users_ddl: "create table users (id integer primary key, email text not null unique, \
                name text not null, age integer not null)",
    inserts_ddl: "create table inserts (id integer primary key, val integer not null)",
    seed_sql: "insert into users (id, email, name, age) values (?, ?, ?, ?)",
    insert_sql: "insert into inserts (id, val) values (?, ?)",
    simple_sql: "select id, email, name, age from users where id = ?",
    medium_sql: "select id, email, name, age from users where age > ? order by age desc limit 10",
}
