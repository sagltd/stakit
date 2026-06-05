//! `stakit-orm` — a high-performance, type-safe, **database-agnostic** ORM,
//! inspired by Drizzle.
//!
//! One typed query builder runs on Postgres, `SQLite`, `MySQL` (all via sqlx), and
//! Turso / `libSQL` (not sqlx) behind the [`Driver`] trait. Backends are opt-in
//! cargo features — `postgres` (default), `sqlite`, `mysql`, `turso` — so you
//! compile only the driver you use.
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

#[doc(hidden)]
pub mod composite;
mod db;
pub mod dialect;
pub mod driver;
pub mod error;
mod exec;
pub mod expr;
pub mod geo;
mod ident;
pub mod insert;
pub mod json;
mod mutation;
pub mod nanoid;
pub mod projection;
mod query;
pub mod raw;
mod render;
pub mod schema;
mod sql;
pub mod value;
pub mod vector;

#[cfg(feature = "postgres")]
pub use db::DbConfig;
pub use db::{Db, Migration, Tx};
pub use dialect::{Dialect, MySqlDialect, PostgresDialect, SqliteDialect, TursoDialect};
#[cfg(feature = "mysql")]
pub use driver::MySqlDriver;
#[cfg(feature = "postgres")]
pub use driver::PostgresDriver;
#[cfg(feature = "sqlite")]
pub use driver::SqliteDriver;
#[cfg(feature = "turso")]
pub use driver::TursoDriver;
pub use driver::{Driver, Row, RowSink, TxConn};
pub use error::{Error, Result};
pub use geo::{
    DistanceUnit, Dms, GeoPoint, Geography, Geometry, st_contains, st_distance, st_dwithin,
    st_intersects, st_within,
};
pub use ident::IdentError;
pub use insert::{ConflictKey, Insert, InsertReturning, Insertable, OptionalPresent, Upsert};
pub use json::Json;
pub use mutation::{Delete, Update};
pub use nanoid::{nanoid, nanoid_custom, nanoid_sized};
pub use projection::{
    Agg, All, Count, NotNull, Nullable, Projection, SqlExpr, TsRank, avg, count, count_col, max,
    min, sql_expr, sum, ts_rank, ts_rank_in, ts_rank_stored,
};
pub use query::Select;
pub use raw::Raw;
pub use schema::{Col, Column, Expr, ForeignKey, OnDelete, Rel, Table};
pub use sql::SqlWriter;
pub use value::{FromValue, ToValue, Value, ValueKind};
pub use vector::{Distance, Vector};

/// The `#[derive(Table)]` macro (shares its name with the [`Table`] trait, like
/// serde's `Serialize`).
pub use stakit_orm_derive::Table;

/// The `#[derive(Row)]` macro: a named projection over `#[from(<expr>)]` fields.
pub use stakit_orm_derive::Row;

/// The `#[derive(Role)]` macro: declares a database role for row-level security
/// migrations (`#[role(name = "...", login, …)]`), exposing `Self::ROLE`.
pub use stakit_orm_derive::Role;

/// The `#[derive(DbEnum)]` macro: makes a fieldless enum a column type (stored as
/// text by default, or as an integer with `#[db_enum(int)]`).
pub use stakit_orm_derive::DbEnum;

/// The `#[derive(Type)]` macro: makes a struct a Postgres composite column type.
pub use stakit_orm_derive::Type;

/// Common imports: the [`Db`] handle, the derive, query operators, and core
/// traits/types.
pub mod prelude {
    pub use crate::Db;
    pub use crate::DbEnum;
    pub use crate::Json;
    pub use crate::Migration;
    pub use crate::Role;
    pub use crate::Table;
    pub use crate::error::{Error, Result};
    pub use crate::expr::{
        ColExpr, Direction, IntoExpr, Order, Predicate, and, any_of, asc, contains, desc, eq, gt,
        gte, is_null, like, lt, lte, matches, matches_in, matches_tsv, matches_tsv_in, ne, not, or,
        raw_pred,
    };
    pub use crate::geo::{
        DistanceUnit, GeoPoint, Geography, Geometry, st_contains, st_distance, st_dwithin,
        st_intersects, st_within,
    };
    pub use crate::insert::Upsert;
    pub use crate::projection::{
        All, Count, Projection, avg, count, count_col, max, min, sql_expr, sum, ts_rank,
        ts_rank_in, ts_rank_stored,
    };
    pub use crate::schema::{Col, Rel, Table as TableTrait};
    pub use crate::vector::{Distance, Vector, distance};
}
