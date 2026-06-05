#![allow(missing_docs)] // test binary
#![cfg(feature = "postgres")]
//! Smoke test: pgvector extension works end-to-end against the embedded
//! Postgres instance used by all other integration tests.
//!
//! If the `vector` extension is not installed in the embedded Postgres, this test
//! **skips** (prints a notice and returns) rather than failing — so the workspace
//! gate stays green on machines/CI without pgvector. Where pgvector *is* installed it
//! runs the full KNN + HNSW assertions.
//!
//! ## Prerequisites (to actually exercise pgvector)
//!
//! The `vector` extension must be installed in the theseus Postgres installation
//! directory (`~/.theseus/postgresql/<version>/`).
//! It is NOT bundled with the theseus binary distribution — you must build and
//! install it once from source:
//!
//! ```sh
//! git clone --depth=1 https://github.com/pgvector/pgvector.git /tmp/pgvector_build
//! cd /tmp/pgvector_build
//! PG_CONFIG=~/.theseus/postgresql/<version>/bin/pg_config \
//!   make "PG_SYSROOT=$(xcrun --show-sdk-path)" OPTFLAGS="" install
//! ```
//!
//! Run this test:
//!
//! ```sh
//! cargo nextest run -p stakit-orm --features postgres -E 'test(pgvector)'
//! ```

use stakit_orm::prelude::*;
use stakit_orm::vector::{Distance, Vector, distance};

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// A documents table with a 3-dimensional embedding column.
#[derive(Table, Debug, Clone, PartialEq)]
#[table(name = "docs_vec")]
#[allow(dead_code)]
struct DocVec {
    #[column(pk)]
    id: i64,
    label: String,
    #[column(sql_type = "vector(3)")]
    embedding: Vector,
}

// ---------------------------------------------------------------------------
// Harness — identical pattern to `postgres_test.rs`
// ---------------------------------------------------------------------------

/// Boot embedded Postgres and enable the `vector` extension. Returns `None` (after a
/// printed notice) when pgvector is not installed, so the test skips instead of failing.
async fn setup_pgvector() -> Option<(postgresql_embedded::PostgreSQL, Db)> {
    let mut postgres = postgresql_embedded::PostgreSQL::default();
    postgres.setup().await.expect("setup embedded postgres");
    postgres.start().await.expect("start embedded postgres");
    postgres
        .create_database("pgvec_test")
        .await
        .expect("create database");
    let url = postgres.settings().url("pgvec_test");
    let db = Db::connect(&url).await.expect("connect");

    // Enable the vector extension. If it is not installed in the theseus pg tree, skip
    // the test rather than fail — keeps the gate green where pgvector is unavailable.
    if let Err(error) = db.raw("create extension if not exists vector").exec().await {
        eprintln!(
            "SKIP pgvector_smoke: `create extension vector` failed ({error}); \
             install pgvector into the embedded pg tree to run this test (see module docs)."
        );
        postgres.stop().await.ok();
        return None;
    }

    // Create the docs_vec table with a vector(3) column.
    db.raw(
        "create table docs_vec \
         (id bigint primary key, label text not null, embedding vector(3) not null)",
    )
    .exec()
    .await
    .expect("create docs_vec table");

    Some((postgres, db))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// `CREATE EXTENSION vector`, insert rows, run L2 and cosine KNN queries, build
/// an HNSW index, and assert correct ordering.
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn pgvector_extension_knn_and_hnsw() {
    let Some((postgres, db)) = setup_pgvector().await else {
        return; // pgvector not installed — skipped (see setup_pgvector).
    };

    // Seed three documents.
    // Row 1 – close to query [1,0,0] in L2 and cosine.
    db.raw("insert into docs_vec (id, label, embedding) values ($1, $2, $3::vector)")
        .bind(1_i64)
        .bind("north".to_owned())
        .bind("[1,0,0]".to_owned())
        .exec()
        .await
        .expect("insert row 1");

    // Row 2 – moderately distant (diagonal).
    db.raw("insert into docs_vec (id, label, embedding) values ($1, $2, $3::vector)")
        .bind(2_i64)
        .bind("diagonal".to_owned())
        .bind("[1,1,0]".to_owned())
        .exec()
        .await
        .expect("insert row 2");

    // Row 3 – farthest (opposite direction).
    db.raw("insert into docs_vec (id, label, embedding) values ($1, $2, $3::vector)")
        .bind(3_i64)
        .bind("south".to_owned())
        .bind("[-1,0,0]".to_owned())
        .exec()
        .await
        .expect("insert row 3");

    // --- L2 nearest-neighbour via the stakit-orm typed builder ---------------

    let query = [1.0_f32, 0.0, 0.0];
    let l2_results = db
        .find::<DocVec>()
        .nearest(DocVec::embedding, &query, Distance::L2)
        .limit(3)
        .all()
        .await
        .expect("L2 nearest-neighbour query");

    assert_eq!(l2_results.len(), 3, "should return all 3 rows");
    assert_eq!(
        l2_results[0].id, 1,
        "row 1 ([1,0,0]) must be closest to query [1,0,0] under L2, got: {}",
        l2_results[0].label
    );
    assert_eq!(
        l2_results[2].id, 3,
        "row 3 ([-1,0,0]) must be farthest under L2, got: {}",
        l2_results[2].label
    );

    // --- Cosine nearest-neighbour via typed builder --------------------------

    let cosine_results = db
        .find::<DocVec>()
        .nearest(DocVec::embedding, &query, Distance::Cosine)
        .limit(3)
        .all()
        .await
        .expect("cosine nearest-neighbour query");

    assert_eq!(cosine_results.len(), 3, "should return all 3 rows");
    // [1,0,0] vs [1,0,0] → cosine distance 0 (identical direction).
    assert_eq!(
        cosine_results[0].id, 1,
        "row 1 must be closest in cosine, got: {}",
        cosine_results[0].label
    );
    // [-1,0,0] vs [1,0,0] → cosine distance 2 (exactly opposite).
    assert_eq!(
        cosine_results[2].id, 3,
        "row 3 must be farthest in cosine, got: {}",
        cosine_results[2].label
    );

    // --- Select with a distance score projection (cosine) --------------------

    let scored: Vec<(i64, f64)> = db
        .select((
            DocVec::id,
            distance(DocVec::embedding, &query, Distance::Cosine),
        ))
        .from::<DocVec>()
        .nearest(DocVec::embedding, &query, Distance::Cosine)
        .limit(1)
        .all()
        .await
        .expect("cosine distance projection");

    assert_eq!(scored.len(), 1);
    let (top_id, top_dist) = scored[0];
    assert_eq!(top_id, 1, "top row must be id=1");
    assert!(
        top_dist < 1e-6,
        "cosine distance of identical vectors must be ~0, got {top_dist}"
    );

    // --- HNSW index: create, confirm it exists, then rerun KNN ---------------

    db.raw(
        "create index idx_docs_vec_embedding \
         on docs_vec using hnsw (embedding vector_cosine_ops)",
    )
    .exec()
    .await
    .expect("create HNSW index");

    // Confirm the index exists in pg_indexes.
    let index_name: String = sqlx::query_scalar(
        "select indexname from pg_indexes \
         where tablename = 'docs_vec' and indexname = 'idx_docs_vec_embedding'",
    )
    .fetch_one(db.pool().expect("pool"))
    .await
    .expect("pg_indexes query");
    assert_eq!(
        index_name, "idx_docs_vec_embedding",
        "HNSW index must be present in pg_indexes"
    );

    // Cosine KNN with the index in place — results must be identical.
    let after_index = db
        .find::<DocVec>()
        .nearest(DocVec::embedding, &query, Distance::Cosine)
        .limit(3)
        .all()
        .await
        .expect("cosine KNN after HNSW index");
    assert_eq!(
        after_index[0].id, 1,
        "top result must not change after index"
    );
    assert_eq!(
        after_index[2].id, 3,
        "tail result must not change after index"
    );

    // --- Round-trip: read back a Vector column value -------------------------

    let row = db
        .find::<DocVec>()
        .filter(eq(DocVec::id, 1_i64))
        .one()
        .await
        .expect("select by id")
        .expect("row must exist");

    assert_eq!(
        row.embedding,
        Vector::new([1.0_f32, 0.0, 0.0]),
        "vector column must round-trip through the ORM decoder"
    );

    // --- Direct <-> and <=> operators in raw SQL -----------------------------

    // L2 (<->) raw SQL.
    let raw_l2_id: i64 =
        sqlx::query_scalar("select id from docs_vec order by embedding <-> $1::vector limit 1")
            .bind("[1,0,0]")
            .fetch_one(db.pool().expect("pool"))
            .await
            .expect("raw <-> query");
    assert_eq!(raw_l2_id, 1, "raw <-> must return id=1 as nearest");

    // Cosine (<=>) raw SQL.
    let raw_cos_id: i64 =
        sqlx::query_scalar("select id from docs_vec order by embedding <=> $1::vector limit 1")
            .bind("[1,0,0]")
            .fetch_one(db.pool().expect("pool"))
            .await
            .expect("raw <=> query");
    assert_eq!(raw_cos_id, 1, "raw <=> must return id=1 as nearest");

    postgres.stop().await.ok();
}
