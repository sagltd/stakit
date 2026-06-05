#![allow(missing_docs)] // gated-out builds compile to an empty crate
#![cfg(feature = "postgres")]
//! End-to-end full-text search against real Postgres, exercising the new
//! capabilities together: a **GIN index** on a generated `tsvector` column (the
//! table-level `index(..., method = "gin")` attribute), a stored-`tsvector`
//! [`matches_tsv`] filter, and the typed [`ts_rank_stored`] relevance projection +
//! [`Select::order_by_rank`] — i.e. "search, ranked by relevance", fully declarative.

use stakit_orm::prelude::*;

#[derive(stakit_orm::Table, Debug)]
#[table(name = "docs", index(idx_docs_tsv = (tsv), method = "gin"))]
#[allow(dead_code)]
struct Doc {
    #[column(pk)]
    id: i64,
    body: String,
    // Generated stored tsvector (omitted from `DocNew`); the GIN index covers it.
    #[column(sql_type = "tsvector", generated = "to_tsvector('english', body)")]
    tsv: String,
}

async fn connect() -> (postgresql_embedded::PostgreSQL, Db) {
    let mut postgres = postgresql_embedded::PostgreSQL::default();
    postgres.setup().await.expect("setup embedded postgres");
    postgres.start().await.expect("start embedded postgres");
    postgres
        .create_database("fts_rank_test")
        .await
        .expect("create database");
    let db = Db::connect(&postgres.settings().url("fts_rank_test"))
        .await
        .expect("connect");
    (postgres, db)
}

#[tokio::test]
async fn full_text_search_with_gin_index_and_ts_rank() {
    let (postgres, db) = connect().await;

    // The DDL `stakit-orm-cli` generates for the table + its GIN index attribute.
    db.raw(
        "create table docs (id bigint primary key, body text not null, \
         tsv tsvector generated always as (to_tsvector('english', body)) stored)",
    )
    .exec()
    .await
    .expect("create table");
    db.raw("create index idx_docs_tsv on docs using gin (tsv)")
        .exec()
        .await
        .expect("create gin index");

    // The generated `tsv` column is omitted from `DocNew` — the database computes it.
    db.insert_many(vec![
        DocNew {
            id: 1,
            body: "the quick brown fox".to_owned(),
        },
        DocNew {
            id: 2,
            body: "a fox, a fox, the clever fox runs".to_owned(),
        },
        DocNew {
            id: 3,
            body: "lazy dog sleeps all day".to_owned(),
        },
    ])
    .exec()
    .await
    .expect("seed docs");

    // Declarative FTS: match `fox` on the stored (GIN-indexed) tsvector, ranked by
    // relevance, returning the id and its score.
    let hits: Vec<(i64, f32)> = db
        .select((Doc::id, ts_rank_stored(Doc::tsv, "fox")))
        .from::<Doc>()
        .filter(matches_tsv(Doc::tsv, "fox"))
        .order_by_rank(ts_rank_stored(Doc::tsv, "fox"))
        .all()
        .await
        .expect("full-text search");

    let ids: Vec<i64> = hits.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids.len(), 2, "only docs mentioning fox match, got {ids:?}");
    assert!(ids.contains(&1) && ids.contains(&2));
    assert!(!ids.contains(&3), "the dog doc must not match `fox`");

    // Ranked highest-first: doc 2 (three `fox`es) outranks doc 1 (one).
    assert_eq!(hits[0].0, 2, "most relevant doc first, got {hits:?}");
    assert!(hits[0].1 >= hits[1].1, "ts_rank descending, got {hits:?}");
    assert!(hits[0].1 > 0.0, "a matching doc has a positive rank");

    postgres.stop().await.ok();
}
