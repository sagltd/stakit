//! `stakit-orm` — a high-performance, type-safe Postgres ORM built on sqlx,
//! inspired by Drizzle.
//!
//! Define a schema once with `#[derive(Table)]`, then build typed queries whose
//! return type is inferred from the projection. See
//! `docs/superpowers/specs/2026-06-02-stakit-orm-design.md` for the full design.
//!
//! ```no_run
//! use stakit_orm::prelude::*;
//!
//! # async fn demo(db: Db, uid: i64) -> stakit_orm::Result<()> {
//! # #[derive(Table)]
//! # #[table(name = "users")]
//! # struct User { #[column(pk)] id: i64, email: String }
//! let user = db.select(User::all())
//!     .from::<User>()
//!     .filter(eq(User::id, uid))
//!     .one()
//!     .await?;
//! # let _ = user;
//! # Ok(())
//! # }
//! ```

mod db;
pub mod error;
mod exec;
pub mod expr;
mod ident;
pub mod insert;
mod mutation;
pub mod nanoid;
pub mod projection;
mod query;
pub mod raw;
mod render;
pub mod schema;
mod sql;

pub use db::{Db, DbConfig, Tx};
pub use error::{Error, Result};
pub use ident::IdentError;
pub use insert::{Insert, InsertReturning, Insertable, OptionalPresent};
pub use mutation::{Delete, Update};
pub use nanoid::{nanoid, nanoid_custom, nanoid_sized};
pub use projection::{
    Agg, All, Count, NotNull, Nullable, Projection, SqlExpr, avg, count, count_col, max, min,
    sql_expr, sum,
};
pub use query::Select;
pub use raw::Raw;
pub use schema::{Col, Column, Expr, ForeignKey, OnDelete, Rel, Table};
pub use sql::{Bind, SqlWriter};

/// The `#[derive(Table)]` macro (shares its name with the [`Table`] trait, like
/// serde's `Serialize`).
pub use stakit_orm_derive::Table;

/// The `#[derive(Row)]` macro: a named projection over `#[from(<expr>)]` fields.
pub use stakit_orm_derive::Row;

/// Common imports: the [`Db`] handle, the derive, query operators, and core
/// traits/types.
pub mod prelude {
    pub use crate::Db;
    pub use crate::Table;
    pub use crate::error::{Error, Result};
    pub use crate::expr::{
        ColExpr, Direction, IntoExpr, Order, Predicate, and, any_of, asc, desc, eq, gt, gte,
        is_null, like, lt, lte, ne, or, raw_pred,
    };
    pub use crate::projection::{
        All, Count, Projection, avg, count, count_col, max, min, sql_expr, sum,
    };
    pub use crate::schema::{Col, Rel, Table as TableTrait};
}
