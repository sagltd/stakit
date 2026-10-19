//! The minimal uuid-keyed `users` table the SQL-rendering tests and benches
//! build queries against. Included with `#[path]`.
#![allow(dead_code, unreachable_pub)]

use stakit_orm::prelude::*;
use uuid::Uuid;

#[derive(Table)]
#[table(name = "users")]
pub struct User {
    #[column(pk)]
    pub id: Uuid,
    #[column(unique)]
    pub email: String,
    pub name: String,
}
