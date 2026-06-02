//! Schema tokens: the [`Table`] trait, typed [`Col`] column tokens, and the
//! static [`Column`] metadata that the derive macro emits.

use core::marker::PhantomData;

/// `ON DELETE` referential action for a foreign key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnDelete {
    /// `ON DELETE CASCADE`.
    Cascade,
    /// `ON DELETE RESTRICT`.
    Restrict,
    /// `ON DELETE SET NULL` (requires a nullable column).
    SetNull,
    /// `ON DELETE NO ACTION` (the Postgres default).
    NoAction,
}

impl OnDelete {
    /// The SQL clause text for this action.
    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            Self::Cascade => "cascade",
            Self::Restrict => "restrict",
            Self::SetNull => "set null",
            Self::NoAction => "no action",
        }
    }
}

/// A foreign-key reference from a column to another table's column.
#[derive(Debug, Clone, Copy)]
pub struct ForeignKey {
    /// Referenced table name.
    pub table: &'static str,
    /// Referenced column name.
    pub column: &'static str,
    /// Referential action on delete.
    pub on_delete: OnDelete,
}

/// Static metadata for one column, emitted by `#[derive(Table)]`.
#[derive(Debug, Clone, Copy)]
pub struct Column {
    /// SQL column name.
    pub name: &'static str,
    /// SQL type (e.g. `uuid`, `text`).
    pub sql_type: &'static str,
    /// Whether this is (part of) the primary key.
    pub is_pk: bool,
    /// Whether a `UNIQUE` constraint applies.
    pub is_unique: bool,
    /// Whether the column is nullable.
    pub is_nullable: bool,
    /// Verbatim SQL `DEFAULT` expression, if any.
    pub default: Option<&'static str>,
    /// Foreign-key reference, if any.
    pub references: Option<ForeignKey>,
}

/// A table mapped from a Rust struct. Implemented by `#[derive(Table)]`.
pub trait Table: for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> + Send + Unpin {
    /// SQL table name.
    const TABLE: &'static str;
    /// Ordered column metadata.
    const COLUMNS: &'static [Column];
    /// The Rust type of the primary key (used for compile-time FK checks).
    type Pk;

    /// Decode this row from columns at **positional** ordinals `start..start+N`
    /// (in [`COLUMNS`](Self::COLUMNS) order). Unlike `FromRow` (by name), this
    /// works inside a join tuple where two tables share column names.
    ///
    /// # Errors
    /// Propagates any sqlx decode error.
    fn from_row_at(row: &sqlx::postgres::PgRow, start: usize) -> sqlx::Result<Self>
    where
        Self: Sized;
}

/// A virtual relation field on a table struct — not a column. Skipped by SQL
/// and migrations; populated by the relational query API.
pub struct Rel<T> {
    marker: PhantomData<fn() -> T>,
}

impl<T> Default for Rel<T> {
    fn default() -> Self {
        Self {
            marker: PhantomData,
        }
    }
}

impl<T> core::fmt::Debug for Rel<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Rel")
    }
}

/// The decoded Rust type produced by an expression (column, aggregate, or raw).
pub trait Expr {
    /// The Rust type one row position decodes to.
    type Out;
}

/// A typed column token: `User::id: Col<User, Uuid>`.
///
/// Carries the table + column names so queries render `"table"."column"` while
/// the `Ty` parameter keeps comparisons type-checked.
pub struct Col<T, Ty> {
    /// Owning table name.
    pub table: &'static str,
    /// Column name.
    pub name: &'static str,
    marker: PhantomData<fn() -> (T, Ty)>,
}

impl<T, Ty> Col<T, Ty> {
    /// Construct a column token (called by generated code).
    #[must_use]
    pub const fn new(table: &'static str, name: &'static str) -> Self {
        Self {
            table,
            name,
            marker: PhantomData,
        }
    }
}

impl<T, Ty> Clone for Col<T, Ty> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T, Ty> Copy for Col<T, Ty> {}

impl<T, Ty> Expr for Col<T, Ty> {
    type Out = Ty;
}
