//! A `things` table mixing `#[derive(DbEnum)]` (text + int), every chrono
//! temporal type, and JSON — round-tripped by each backend suite. Included with
//! `#[path]`.
#![allow(dead_code, unreachable_pub)]

use chrono::{NaiveDate, NaiveTime, TimeZone, Utc};
use stakit_orm::prelude::*;

#[derive(DbEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    A,
    B,
}

#[derive(DbEnum, Debug, Clone, Copy, PartialEq, Eq)]
#[db_enum(int)]
pub enum Rank {
    Low = 1,
    High = 9,
}

#[derive(Table, Debug)]
#[table(name = "things")]
pub struct Thing {
    #[column(pk)]
    pub id: i64,
    #[column(sql_type = "text")]
    pub kind: Kind,
    #[column(sql_type = "int")]
    pub rank: Rank,
    pub at: chrono::DateTime<chrono::Utc>,
    pub local: chrono::NaiveDateTime,
    pub day: chrono::NaiveDate,
    pub alarm: chrono::NaiveTime,
    pub meta: serde_json::Value,
}

/// Insert one thing into an empty `things` table, read it back by primary key,
/// check every column survived, and filter on an enum column.
pub async fn assert_round_trips(db: &Db) {
    let at = Utc.with_ymd_and_hms(2026, 6, 2, 8, 30, 0).unwrap();
    let day = NaiveDate::from_ymd_opt(1990, 1, 15).unwrap();
    let alarm = NaiveTime::from_hms_opt(8, 0, 0).unwrap();
    let local = day.and_hms_opt(8, 30, 0).unwrap();
    let meta = serde_json::json!({ "k": [1, 2, 3], "ok": true });

    db.insert(ThingNew {
        id: 1,
        kind: Kind::B,
        rank: Rank::High,
        at,
        local,
        day,
        alarm,
        meta: meta.clone(),
    })
    .exec()
    .await
    .expect("insert thing");

    let got = db.get::<Thing>(1).one().await.unwrap().unwrap();
    assert_eq!(got.kind, Kind::B);
    assert_eq!(got.rank, Rank::High);
    assert_eq!(got.at, at);
    assert_eq!(got.local, local);
    assert_eq!(got.day, day);
    assert_eq!(got.alarm, alarm);
    assert_eq!(got.meta, meta);

    // filter on an enum column.
    let highs = db
        .find::<Thing>()
        .filter(eq(Thing::rank, Rank::High))
        .all()
        .await
        .unwrap();
    assert_eq!(highs.len(), 1);
}
