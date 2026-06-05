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

/// A table privilege grantable to a role (`GRANT <privilege> ON …`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Privilege {
    /// `ALL PRIVILEGES`.
    All,
    /// `SELECT`.
    Select,
    /// `INSERT`.
    Insert,
    /// `UPDATE`.
    Update,
    /// `DELETE`.
    Delete,
}

impl Privilege {
    /// The SQL keyword(s) for this privilege (`All` renders `all privileges`).
    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            Self::All => "all privileges",
            Self::Select => "select",
            Self::Insert => "insert",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }
}

/// The command a row-level-security policy applies to (`CREATE POLICY … FOR <command>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyCommand {
    /// `FOR ALL` (every command).
    All,
    /// `FOR SELECT`.
    Select,
    /// `FOR INSERT`.
    Insert,
    /// `FOR UPDATE`.
    Update,
    /// `FOR DELETE`.
    Delete,
}

impl PolicyCommand {
    /// The SQL keyword for this command.
    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Select => "select",
            Self::Insert => "insert",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }
}

/// A row-level-security policy on a table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// Policy name (unique per table).
    pub name: String,
    /// Command the policy governs.
    pub command: PolicyCommand,
    /// Roles the policy applies to. Empty means `PUBLIC` (no `TO` clause emitted).
    #[serde(default)]
    pub roles: Vec<String>,
    /// `USING (<expr>)` visibility predicate, if any. Verbatim trusted SQL.
    #[serde(default)]
    pub using: Option<String>,
    /// `WITH CHECK (<expr>)` write predicate, if any. Verbatim trusted SQL.
    #[serde(default)]
    pub check: Option<String>,
}

/// A privilege grant to a single role on a table. A multi-role declaration is
/// stored as one entry per role so diffs key cleanly on the role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    /// The role receiving the privileges.
    pub role: String,
    /// Privileges granted, canonicalized (sorted, deduplicated).
    pub privileges: Vec<Privilege>,
}

/// A database role (`CREATE ROLE`). Passwords are never modeled — they must be set
/// out of band, never written into a migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)] // role attribute flags (login/createdb/createrole/bypassrls)
pub struct Role {
    /// Role name.
    pub name: String,
    /// `LOGIN` (default `NOLOGIN`).
    #[serde(default)]
    pub login: bool,
    /// `CREATEDB`.
    #[serde(default)]
    pub createdb: bool,
    /// `CREATEROLE`.
    #[serde(default)]
    pub createrole: bool,
    /// `BYPASSRLS` (the role is exempt from row-level security).
    #[serde(default)]
    pub bypassrls: bool,
}

/// One table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Table {
    /// Table name.
    pub name: String,
    /// Columns, in declaration order.
    pub columns: Vec<Column>,
    /// Whether `ROW LEVEL SECURITY` is enabled on the table.
    #[serde(default)]
    pub rls: bool,
    /// Whether RLS is `FORCE`d (applies even to the table owner). Requires [`rls`](Self::rls).
    #[serde(default)]
    pub force_rls: bool,
    /// Row-level-security policies, sorted by name for stable diffs.
    #[serde(default)]
    pub policies: Vec<Policy>,
    /// Privilege grants, one per role, sorted by role for stable diffs.
    #[serde(default)]
    pub grants: Vec<Grant>,
}

impl Table {
    /// Find a column by name.
    #[must_use]
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|column| column.name == name)
    }

    /// Find a policy by name.
    #[must_use]
    pub fn policy(&self, name: &str) -> Option<&Policy> {
        self.policies.iter().find(|policy| policy.name == name)
    }

    /// Find the grant for a role.
    #[must_use]
    pub fn grant(&self, role: &str) -> Option<&Grant> {
        self.grants.iter().find(|grant| grant.role == role)
    }
}

/// A whole-schema snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schema {
    /// Tables, sorted by name for stable diffs.
    pub tables: Vec<Table>,
    /// Database roles, sorted by name for stable diffs.
    #[serde(default)]
    pub roles: Vec<Role>,
}

impl Schema {
    /// Find a table by name.
    #[must_use]
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.iter().find(|table| table.name == name)
    }

    /// Find a role by name.
    #[must_use]
    pub fn role(&self, name: &str) -> Option<&Role> {
        self.roles.iter().find(|role| role.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::{Grant, Policy, PolicyCommand, Privilege, Role, Schema, Table};

    /// A snapshot written before RLS existed (no `roles`, no per-table RLS fields)
    /// must still load — the new fields default, so old snapshots are not a hard error.
    #[test]
    fn pre_rls_snapshot_deserializes_with_defaults() {
        let json = r#"{
            "tables": [
                { "name": "users",
                  "columns": [
                    { "name": "id", "sql_type": "uuid", "nullable": false, "pk": true,
                      "unique": false, "default": null, "references": null }
                  ] }
            ]
        }"#;
        let schema: Schema = serde_json::from_str(json).expect("load pre-RLS snapshot");
        let users = schema.table("users").expect("users table");
        assert!(schema.roles.is_empty());
        assert!(!users.rls);
        assert!(!users.force_rls);
        assert!(users.policies.is_empty());
        assert!(users.grants.is_empty());
    }

    #[test]
    fn rls_schema_round_trips_through_json() {
        let schema = Schema {
            tables: vec![Table {
                name: "posts".to_owned(),
                columns: Vec::new(),
                rls: true,
                force_rls: true,
                policies: vec![Policy {
                    name: "posts_owner".to_owned(),
                    command: PolicyCommand::Select,
                    roles: vec!["app_user".to_owned()],
                    using: Some("author_id = current_user_id()".to_owned()),
                    check: None,
                }],
                grants: vec![Grant {
                    role: "app_user".to_owned(),
                    privileges: vec![Privilege::Select, Privilege::Insert],
                }],
            }],
            roles: vec![Role {
                name: "app_user".to_owned(),
                login: true,
                createdb: false,
                createrole: false,
                bypassrls: false,
            }],
        };
        let json = serde_json::to_string(&schema).expect("serialize");
        let parsed: Schema = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(schema, parsed);
    }

    #[test]
    fn privileges_order_canonically() {
        // Ord is the diff's canonical sort; `All` precedes the specific privileges.
        let mut privileges = vec![
            Privilege::Delete,
            Privilege::Select,
            Privilege::All,
            Privilege::Insert,
        ];
        privileges.sort_unstable();
        assert_eq!(
            privileges,
            vec![
                Privilege::All,
                Privilege::Select,
                Privilege::Insert,
                Privilege::Delete
            ]
        );
    }
}
