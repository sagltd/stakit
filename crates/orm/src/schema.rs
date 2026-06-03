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
#[allow(clippy::struct_excessive_bools)] // column flags (pk/unique/index/nullable)
pub struct Column {
    /// SQL column name.
    pub name: &'static str,
    /// SQL type (e.g. `uuid`, `text`).
    pub sql_type: &'static str,
    /// Whether this column is the (single-column) primary key. Composite primary
    /// keys are rejected by the derive.
    pub is_pk: bool,
    /// Whether a `UNIQUE` constraint applies.
    pub is_unique: bool,
    /// Whether a (non-unique) secondary index should be created on this column.
    pub is_index: bool,
    /// The index access method, when one was requested explicitly (e.g. `"gist"`
    /// for a `PostGIS` geometry column via `#[column(index = "gist")]`). `None` uses
    /// the backend default (B-tree). Only meaningful when [`is_index`](Self::is_index).
    pub index_method: Option<&'static str>,
    /// Whether the column is nullable.
    pub is_nullable: bool,
    /// Verbatim SQL `DEFAULT` expression, if any.
    pub default: Option<&'static str>,
    /// Foreign-key reference, if any.
    pub references: Option<ForeignKey>,
    /// A cast applied when this column is **selected** in a whole-row projection
    /// (e.g. `Some("text")` for a Postgres composite read as `col::text`). `None`
    /// selects the column bare. Sourced from the field type's
    /// [`FromValue::READ_CAST`](crate::FromValue::READ_CAST).
    pub read_cast: Option<&'static str>,
}

impl Column {
    /// Render the `CREATE INDEX` DDL for this column on `table`, or `None` when the
    /// column has no secondary index ([`is_index`](Self::is_index) is `false`).
    ///
    /// Emits `create index "idx_<table>_<name>" on "<table>" using <method> ("<name>")`
    /// when an [`index_method`](Self::index_method) is set (e.g. `gist` for a `PostGIS`
    /// geometry column), or the same without the `using` clause for the default
    /// B-tree. Identifiers are quoted; the method is a developer-supplied
    /// `&'static str` written verbatim.
    ///
    /// ```
    /// # use stakit_orm::Column;
    /// let geo = Column {
    ///     name: "location", sql_type: "geometry(Point,4326)", is_pk: false,
    ///     is_unique: false, is_index: true, index_method: Some("gist"),
    ///     is_nullable: false, default: None, references: None, read_cast: None,
    /// };
    /// assert_eq!(
    ///     geo.create_index_sql("places").as_deref(),
    ///     Some(r#"create index "idx_places_location" on "places" using gist ("location")"#),
    /// );
    /// ```
    #[must_use]
    pub fn create_index_sql(&self, table: &str) -> Option<String> {
        if !self.is_index {
            return None;
        }
        let using = self
            .index_method
            .map_or_else(String::new, |method| format!(" using {method}"));
        Some(format!(
            r#"create index "idx_{table}_{name}" on "{table}"{using} ("{name}")"#,
            name = self.name,
        ))
    }
}

/// A table mapped from a Rust struct. Implemented by `#[derive(Table)]`.
pub trait Table: Send + Unpin {
    /// SQL table name.
    const TABLE: &'static str;
    /// Ordered column metadata.
    const COLUMNS: &'static [Column];
    /// The Rust type of the primary key (used for compile-time FK checks).
    type Pk;

    /// Decode this row from columns at **positional** ordinals `start..start+N`
    /// (in [`COLUMNS`](Self::COLUMNS) order), reading each cell through the
    /// backend-neutral [`Row`](crate::driver::Row). Positional (not by-name) so it
    /// works inside a join tuple where two tables share column names.
    ///
    /// # Errors
    /// Propagates any decode error.
    fn from_row_at(row: &dyn crate::driver::Row, start: usize) -> crate::error::Result<Self>
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
