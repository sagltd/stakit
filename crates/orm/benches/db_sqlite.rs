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

use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, Ordering};

use sqlx::SqlitePool;
use sqlx::sqlite::SqlitePoolOptions;
use stakit_orm::Db;
use stakit_orm::prelude::*;

#[global_allocator]
static ALLOC: divan::AllocProfiler = divan::AllocProfiler::system();

fn main() {
    divan::main();
}

#[derive(Table, Debug)]
#[table(name = "users")]
#[allow(dead_code)]
struct User {
    #[column(pk)]
    id: i64,
    #[column(unique)]
    email: String,
    name: String,
    age: i32,
}

#[derive(Table, Debug)]
#[table(name = "inserts")]
#[allow(dead_code)]
struct Ins {
    #[column(pk)]
    id: i64,
    val: i32,
}

const SEED_ROWS: i64 = 1000;
static NEXT_ID: AtomicI64 = AtomicI64::new(1_000_000);

struct Ctx {
    rt: tokio::runtime::Runtime,
    pool: SqlitePool,
    db: Db,
}

fn ctx() -> &'static Ctx {
    static CTX: OnceLock<Ctx> = OnceLock::new();
    CTX.get_or_init(|| {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        let pool = rt.block_on(async {
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect("sqlite::memory:")
                .await
                .expect("connect");
            sqlx::query(
                "create table users (id integer primary key, email text not null unique, \
                 name text not null, age integer not null)",
            )
            .execute(&pool)
            .await
            .expect("users table");
            sqlx::query("create table inserts (id integer primary key, val integer not null)")
                .execute(&pool)
                .await
                .expect("inserts table");
            for i in 0..SEED_ROWS {
                sqlx::query("insert into users (id, email, name, age) values (?, ?, ?, ?)")
                    .bind(i)
                    .bind(format!("u{i}@x.com"))
                    .bind(format!("user{i}"))
                    .bind((i % 80) as i32)
                    .execute(&pool)
                    .await
                    .expect("seed");
            }
            pool
        });
        let db = Db::sqlite(pool.clone());
        Ctx { rt, pool, db }
    })
}

// --- insert ---------------------------------------------------------------

#[divan::bench]
fn raw_insert(bencher: divan::Bencher<'_, '_>) {
    let c = ctx();
    bencher.bench(|| {
        c.rt.block_on(async {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            sqlx::query("insert into inserts (id, val) values (?, ?)")
                .bind(id)
                .bind(1_i32)
                .execute(&c.pool)
                .await
                .expect("raw insert");
        });
    });
}

#[divan::bench]
fn orm_insert(bencher: divan::Bencher<'_, '_>) {
    let c = ctx();
    bencher.bench(|| {
        c.rt.block_on(async {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            c.db.insert(InsNew { id, val: 1 })
                .exec()
                .await
                .expect("orm insert");
        });
    });
}

// --- simple point-select (by primary key) ---------------------------------

#[divan::bench]
fn raw_simple(bencher: divan::Bencher<'_, '_>) {
    let c = ctx();
    bencher.bench(|| {
        c.rt.block_on(async {
            let row: Option<(i64, String, String, i32)> =
                sqlx::query_as("select id, email, name, age from users where id = ?")
                    .bind(500_i64)
                    .fetch_optional(&c.pool)
                    .await
                    .expect("raw simple");
            divan::black_box(row)
        })
    });
}

#[divan::bench]
fn orm_simple(bencher: divan::Bencher<'_, '_>) {
    let c = ctx();
    bencher.bench(|| {
        c.rt.block_on(async {
            let row =
                c.db.select(User::all())
                    .from::<User>()
                    .filter(eq(User::id, 500_i64))
                    .one()
                    .await
                    .expect("orm simple");
            divan::black_box(row)
        })
    });
}

// --- medium: filter + order + limit ---------------------------------------

#[divan::bench]
fn raw_medium(bencher: divan::Bencher<'_, '_>) {
    let c = ctx();
    bencher.bench(|| {
        c.rt.block_on(async {
            let rows: Vec<(i64, String, String, i32)> = sqlx::query_as(
                "select id, email, name, age from users where age > ? order by age desc limit 10",
            )
            .bind(40_i32)
            .fetch_all(&c.pool)
            .await
            .expect("raw medium");
            divan::black_box(rows)
        })
    });
}

#[divan::bench]
fn orm_medium(bencher: divan::Bencher<'_, '_>) {
    let c = ctx();
    bencher.bench(|| {
        c.rt.block_on(async {
            let rows =
                c.db.find::<User>()
                    .filter(gt(User::age, 40))
                    .order_by(desc(User::age))
                    .limit(10)
                    .all()
                    .await
                    .expect("orm medium");
            divan::black_box(rows)
        })
    });
}
