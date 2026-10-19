//! The raw-`sqlx` vs `stakit-orm` benches shared by the per-backend `db_*`
//! benches: the same tables, seed, and three query shapes (insert, simple
//! point-select, medium filtered/ordered/limited select). Each backend supplies
//! its connection setup, DDL, and placeholder syntax; the bench bodies are
//! generated once here.

/// Generate the tables, the lazily-seeded bench context, and the six benches.
///
/// `connect` is an `async` block yielding `(guard, pool, db)`; the guard keeps
/// any embedded server alive for the process lifetime. The SQL strings use the
/// backend's own placeholder syntax.
macro_rules! db_benches {
    (
        pool: $pool:ty,
        connect: $connect:expr,
        users_ddl: $users_ddl:expr,
        inserts_ddl: $inserts_ddl:expr,
        seed_sql: $seed_sql:expr,
        insert_sql: $insert_sql:expr,
        simple_sql: $simple_sql:expr,
        medium_sql: $medium_sql:expr $(,)?
    ) => {
        use std::sync::OnceLock;
        use std::sync::atomic::{AtomicI64, Ordering};

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
            _guard: Box<dyn std::any::Any + Send + Sync>,
            pool: $pool,
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
                let (guard, pool, db) = rt.block_on(async {
                    let (guard, pool, db): (Box<dyn std::any::Any + Send + Sync>, $pool, Db) =
                        $connect.await;
                    sqlx::query($users_ddl)
                        .execute(&pool)
                        .await
                        .expect("users table");
                    sqlx::query($inserts_ddl)
                        .execute(&pool)
                        .await
                        .expect("inserts table");
                    for i in 0..SEED_ROWS {
                        sqlx::query($seed_sql)
                            .bind(i)
                            .bind(format!("u{i}@x.com"))
                            .bind(format!("user{i}"))
                            .bind((i % 80) as i32)
                            .execute(&pool)
                            .await
                            .expect("seed");
                    }
                    (guard, pool, db)
                });
                Ctx {
                    rt,
                    _guard: guard,
                    pool,
                    db,
                }
            })
        }

        // --- insert -----------------------------------------------------------

        #[divan::bench]
        fn raw_insert(bencher: divan::Bencher<'_, '_>) {
            let c = ctx();
            bencher.bench(|| {
                c.rt.block_on(async {
                    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
                    sqlx::query($insert_sql)
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

        // --- simple point-select (by primary key) -----------------------------

        #[divan::bench]
        fn raw_simple(bencher: divan::Bencher<'_, '_>) {
            let c = ctx();
            bencher.bench(|| {
                c.rt.block_on(async {
                    let row: Option<(i64, String, String, i32)> = sqlx::query_as($simple_sql)
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

        // --- medium: filter + order + limit -----------------------------------

        #[divan::bench]
        fn raw_medium(bencher: divan::Bencher<'_, '_>) {
            let c = ctx();
            bencher.bench(|| {
                c.rt.block_on(async {
                    let rows: Vec<(i64, String, String, i32)> = sqlx::query_as($medium_sql)
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
    };
}
