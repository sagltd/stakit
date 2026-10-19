//! The "remember this device" composite-key upsert scenario, shared by every
//! backend suite. Included with `#[path]`.
//!
//! One row per (`user_id`, `device_id`). First login inserts; later logins update
//! only the chosen columns. `set_coalesce(location)` keeps the stored location
//! when a later login's location is still NULL (async geolocation not yet
//! resolved), so a previously-learned location is never wiped — all in ONE
//! atomic statement.
#![allow(dead_code, unreachable_pub)]

use stakit_orm::prelude::*;

#[derive(Table, Debug)]
#[table(name = "login_devices")]
pub struct LoginDevice {
    #[column(pk)]
    pub id: i64,
    pub user_id: i64,
    #[column(sql_type = "text")]
    pub device_id: String,
    pub platform: String,
    #[column(nullable)]
    pub location: Option<String>,
}

/// A login of user 7 from `device_id`.
pub fn login(id: i64, device_id: &str, platform: &str, location: Option<&str>) -> LoginDeviceNew {
    LoginDeviceNew {
        id,
        user_id: 7,
        device_id: device_id.to_owned(),
        platform: platform.to_owned(),
        location: location.map(str::to_owned),
    }
}

/// The reusable "remember this device" upsert: key on (`user_id`, `device_id`),
/// refresh platform, and keep an existing location when the new one is NULL.
pub async fn remember(db: &Db, row: LoginDeviceNew) -> stakit_orm::Result<u64> {
    db.insert(row)
        .on_conflict((LoginDevice::user_id, LoginDevice::device_id))
        .set(LoginDevice::platform)
        .set_coalesce(LoginDevice::location)
        .exec()
        .await
}

/// Run the scenario against an empty `login_devices` table (with a unique
/// (`user_id`, `device_id`) key).
pub async fn assert_remembers_device(db: &Db) {
    // Mon: first login from the phone — geolocation not resolved yet (NULL).
    remember(db, login(1, "phone", "ios-16", None))
        .await
        .unwrap();

    // Async geolocation resolves a little later -> Berlin.
    db.raw("update login_devices set location = 'Berlin' where id = 1")
        .exec()
        .await
        .unwrap();

    // Tue: same phone logs in again (a fresh candidate id; the conflict updates
    // the existing row instead), platform refreshed, location still NULL.
    remember(db, login(2, "phone", "ios-17", None))
        .await
        .unwrap();

    // Exactly one row, platform refreshed, and "Berlin" PRESERVED (not wiped).
    let rows = db.find::<LoginDevice>().all().await.unwrap();
    assert_eq!(rows.len(), 1, "exactly one device row per (user, device)");
    assert_eq!(rows[0].id, 1, "the original row is updated in place");
    assert_eq!(rows[0].platform, "ios-17", "platform refreshed");
    assert_eq!(
        rows[0].location.as_deref(),
        Some("Berlin"),
        "coalesce kept the previously-learned location"
    );

    // Wed: a login that DID resolve a location overwrites the stored one.
    remember(db, login(3, "phone", "ios-17", Some("Munich")))
        .await
        .unwrap();
    let rows = db.find::<LoginDevice>().all().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].location.as_deref(),
        Some("Munich"),
        "a non-NULL incoming location overwrites"
    );
}

/// `login_devices` DDL for the `SQLite`-family backends (`SQLite`, Turso).
pub const SQLITE_DDL: &str = "create table login_devices (id integer primary key, \
    user_id integer not null, device_id text not null, platform text not null, \
    location text, unique(user_id, device_id))";
