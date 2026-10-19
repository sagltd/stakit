#![allow(missing_docs)] // gated-out builds compile to an empty crate
#![cfg(feature = "postgres")]
//! Arithmetic operands (`add` / `sub`) in `UPDATE ... SET` and in comparison
//! guards, against a real embedded Postgres: the atomic "increment if there is
//! room" pattern money code relies on.

use stakit_orm::Update;
use stakit_orm::prelude::*;

#[derive(Table, Debug, Clone)]
#[table(name = "counters")]
struct Counter {
    #[column(pk)]
    id: String,
    balance: i64,
    cap: i64,
}

async fn setup() -> (postgresql_embedded::PostgreSQL, Db) {
    let mut postgres = postgresql_embedded::PostgreSQL::default();
    postgres.setup().await.expect("setup embedded postgres");
    postgres.start().await.expect("start embedded postgres");
    postgres
        .create_database("arith_test")
        .await
        .expect("create database");
    let db = Db::connect(&postgres.settings().url("arith_test"))
        .await
        .expect("connect");
    db.raw(
        "create table counters (id text primary key, balance bigint not null, cap bigint not null)",
    )
    .exec()
    .await
    .expect("create table");
    db.raw("insert into counters values ('a', 10, 100)")
        .exec()
        .await
        .expect("seed");
    (postgres, db)
}

async fn balance(db: &Db) -> i64 {
    let row = db
        .get::<Counter>("a".to_owned())
        .one()
        .await
        .expect("query")
        .expect("row");
    assert_eq!((row.id.as_str(), row.cap), ("a", 100), "seed row unchanged");
    row.balance
}

#[test]
fn add_renders_a_parenthesized_bound_expression() {
    let sql = Update::<Counter>::new()
        .set(Counter::balance, add(Counter::balance, 5_i64))
        .to_sql()
        .expect("sql");
    assert_eq!(
        sql,
        r#"update "counters" set "balance" = ("counters"."balance" + $1)"#
    );
}

#[test]
fn nested_arithmetic_renders_left_to_right() {
    let sql = Update::<Counter>::new()
        .set(
            Counter::balance,
            sub(add(Counter::balance, 5_i64), Counter::cap),
        )
        .to_sql()
        .expect("sql");
    assert_eq!(
        sql,
        r#"update "counters" set "balance" = (("counters"."balance" + $1) - "counters"."cap")"#
    );
}

#[tokio::test]
async fn increment_and_decrement_apply_atomically() {
    let (_pg, db) = setup().await;
    db.update::<Counter>()
        .set(Counter::balance, add(Counter::balance, 32_i64))
        .filter(eq(Counter::id, "a"))
        .exec()
        .await
        .expect("increment");
    db.update::<Counter>()
        .set(Counter::balance, sub(Counter::balance, 2_i64))
        .filter(eq(Counter::id, "a"))
        .exec()
        .await
        .expect("decrement");
    assert_eq!(balance(&db).await, 40);
}

#[tokio::test]
async fn guarded_increment_only_applies_while_there_is_room() {
    let (_pg, db) = setup().await;
    let reserve = |amount: i64| {
        db.update::<Counter>()
            .set(Counter::balance, add(Counter::balance, amount))
            .filter(and(
                eq(Counter::id, "a"),
                gte(Counter::cap, add(Counter::balance, amount)),
            ))
            .exec()
    };
    assert_eq!(reserve(90).await.expect("fits"), 1);
    assert_eq!(reserve(1).await.expect("full"), 0);
    assert_eq!(balance(&db).await, 100);
}

#[tokio::test]
async fn bigint_sum_decodes_as_i64_and_groups_by_an_expression() {
    let (_pg, db) = setup().await;
    db.raw("insert into counters values ('b', 9000000000, 1), ('c', 5, 2)")
        .exec()
        .await
        .expect("seed");
    let total: Option<i64> = db
        .select(sum(Counter::balance))
        .from::<Counter>()
        .one()
        .await
        .expect("sum")
        .expect("row");
    assert_eq!(total, Some(9_000_000_015));
    let mut buckets: Vec<(i64, Option<i64>)> = db
        .select((
            sql_expr::<i64>("(\"counters\".\"cap\" % 2)"),
            sum(Counter::balance),
        ))
        .from::<Counter>()
        .group_by_sql("(\"counters\".\"cap\" % 2)")
        .all()
        .await
        .expect("grouped");
    buckets.sort_unstable();
    assert_eq!(buckets, [(0, Some(15)), (1, Some(9_000_000_000))]);
}

#[tokio::test]
async fn upsert_set_add_inserts_new_rows_and_increments_existing_ones_in_one_statement() {
    let (_postgres, db) = setup().await;
    let inserted = db
        .insert_many(vec![
            CounterNew {
                id: "a".to_owned(),
                balance: 5,
                cap: 0,
            },
            CounterNew {
                id: "b".to_owned(),
                balance: 7,
                cap: 50,
            },
        ])
        .on_conflict(Counter::id)
        .set_add(Counter::balance)
        .exec()
        .await
        .expect("upsert");
    assert_eq!(inserted, 2);
    assert_eq!(balance(&db).await, 15, "existing row incremented, cap kept");
    let fresh = db
        .get::<Counter>("b".to_owned())
        .one()
        .await
        .expect("query")
        .expect("row");
    assert_eq!(
        (fresh.balance, fresh.cap),
        (7, 50),
        "new row inserted as is"
    );
}
