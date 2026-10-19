//! Raw `sqlx` vs `stakit-orm` — latency **and allocations** on real (embedded)
//! Postgres. Same three shapes as the `SQLite` bench: insert, simple point-select,
//! medium filtered/ordered/limited select. Reads run against a fixed 1000-row
//! table; inserts use a separate table.
//!
//! Boots `postgresql_embedded` once (no Docker). `divan::AllocProfiler` reports
//! bytes + allocations per op.
#![cfg(feature = "postgres")]
#![allow(missing_docs)]

#[macro_use]
#[path = "common/db.rs"]
mod common;

db_benches! {
    pool: sqlx::PgPool,
    connect: async {
        let mut pg = postgresql_embedded::PostgreSQL::default();
        pg.setup().await.expect("setup embedded postgres");
        pg.start().await.expect("start embedded postgres");
        pg.create_database("bench").await.expect("create database");
        let url = pg.settings().url("bench");
        let db = Db::connect(&url).await.expect("connect");
        let pool = db.pool().expect("postgres pool").clone();
        (Box::new(pg) as Box<dyn std::any::Any + Send + Sync>, pool, db)
    },
    users_ddl: "create table users (id bigint primary key, email text not null unique, \
                name text not null, age int not null)",
    inserts_ddl: "create table inserts (id bigint primary key, val int not null)",
    seed_sql: "insert into users (id, email, name, age) values ($1, $2, $3, $4)",
    insert_sql: "insert into inserts (id, val) values ($1, $2)",
    simple_sql: "select id, email, name, age from users where id = $1",
    medium_sql: "select id, email, name, age from users where age > $1 order by age desc limit 10",
}
