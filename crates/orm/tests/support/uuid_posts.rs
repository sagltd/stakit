//! A uuid-keyed `posts` table whose author references the including suite's own
//! `User` table (`super::User`), which differs per suite. Included with `#[path]`.
#![allow(dead_code, unreachable_pub)]

use super::User;
use stakit_orm::prelude::*;
use uuid::Uuid;

#[derive(Table, Debug)]
#[table(name = "posts")]
pub struct Post {
    #[column(pk)]
    pub id: Uuid,
    #[column(references = User::id, on_delete = "cascade")]
    pub author_id: Uuid,
    pub title: String,
    pub views: i32,
}
