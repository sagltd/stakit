//! Serializable schema model — the snapshot the diff compares against.

use serde::{Deserialize, Serialize};

/// `ON DELETE` action, stored as its SQL keyword.
pub type OnDelete = String;

/// A foreign-key reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKey {
    /// Referenced table.
    pub table: String,
    /// Referenced column.
    pub column: String,
    /// `ON DELETE` action keyword.
    pub on_delete: OnDelete,
}

/// One column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)] // column flags (pk/unique/index/nullable)
pub struct Column {
    /// Column name.
    pub name: String,
    /// SQL type (e.g. `uuid`, `text`).
    pub sql_type: String,
    /// Whether nullable.
    pub nullable: bool,
    /// Whether part of the primary key.
    pub pk: bool,
    /// Whether unique.
    pub unique: bool,
    /// Whether a (non-unique) secondary index should be created on this column.
    #[serde(default)]
    pub index: bool,
    /// Index access method (e.g. `hnsw`, `gin`, `gist`), if requested. `None` is the
    /// backend default (B-tree). Only meaningful when [`index`](Self::index) is set.
    #[serde(default)]
    pub index_method: Option<String>,
    /// Operator class on the indexed column (e.g. `vector_cosine_ops`), if requested.
    /// Only meaningful when [`index`](Self::index) is set.
    #[serde(default)]
    pub opclass: Option<String>,
    /// `GENERATED ALWAYS AS (<expr>) STORED` expression, if any (e.g. a stored
    /// tsvector). The database computes the value, so it is omitted from inserts.
    #[serde(default)]
    pub generated: Option<String>,
    /// SQL `DEFAULT` expression, if any.
    pub default: Option<String>,
    /// Foreign-key reference, if any.
    pub references: Option<ForeignKey>,
}

impl Column {
    /// Whether the type/nullability differ from `other` (a column of the same name).
    #[must_use]
    pub fn type_differs(&self, other: &Self) -> bool {
        self.sql_type != other.sql_type || self.nullable != other.nullable
    }
}

/// One table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Table {
    /// Table name.
    pub name: String,
    /// Columns, in declaration order.
    pub columns: Vec<Column>,
}

impl Table {
    /// Find a column by name.
    #[must_use]
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|column| column.name == name)
    }
}

/// A whole-schema snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schema {
    /// Tables, sorted by name for stable diffs.
    pub tables: Vec<Table>,
}

impl Schema {
    /// Find a table by name.
    #[must_use]
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.iter().find(|table| table.name == name)
    }
}
