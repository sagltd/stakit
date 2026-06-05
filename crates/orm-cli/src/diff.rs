//! Schema diffing and SQL generation. Pure functions (a [`Resolver`] supplies
//! interactive decisions), so the core is unit-testable without stdin.

use crate::model::{Column, Index, Policy, Privilege, Role, Schema, Table};
use core::fmt::Write as _;

/// A single schema change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Create a new table (with its indexes, grants, RLS state, and policies).
    CreateTable(Table),
    /// Drop a table (full definition kept for the down migration).
    DropTable(Table),
    /// Add a column.
    AddColumn { table: String, column: Column },
    /// Drop a column (old definition kept for the down migration).
    DropColumn { table: String, column: Column },
    /// Change a column's type/nullability.
    AlterType {
        table: String,
        from: Column,
        to: Column,
    },
    /// Rename a column.
    RenameColumn {
        table: String,
        from: String,
        to: String,
        to_def: Column,
        from_def: Column,
    },
    /// Create a database role.
    CreateRole(Role),
    /// Drop a role (full definition kept for the down migration).
    DropRole(Role),
    /// Change a role's attributes (the old state is kept for the down migration).
    AlterRole { from: Role, to: Role },
    /// Enable or disable row-level security on an existing table.
    SetRls { table: String, enabled: bool },
    /// Force (or un-force) row-level security on an existing table.
    SetForceRls { table: String, forced: bool },
    /// Grant privileges to a role on a table.
    Grant {
        table: String,
        role: String,
        privileges: Vec<Privilege>,
    },
    /// Revoke privileges from a role on a table (privileges kept for the down migration).
    Revoke {
        table: String,
        role: String,
        privileges: Vec<Privilege>,
    },
    /// Create a row-level-security policy.
    CreatePolicy { table: String, policy: Policy },
    /// Drop a policy (full definition kept for the down migration).
    DropPolicy { table: String, policy: Policy },
    /// Create a table-level (composite/unique/method/partial) index.
    CreateIndex { table: String, index: Index },
    /// Drop a table-level index (full definition kept for the down migration).
    DropIndex { table: String, index: Index },
}

/// Supplies interactive decisions during diffing.
pub trait Resolver {
    /// Given a newly added column and the columns removed from the same table,
    /// return `Some(removed_name)` to treat it as a **rename** of that column, or
    /// `None` to add it as a brand-new field (the removed columns become drops).
    fn rename_target(
        &mut self,
        table: &str,
        added: &Column,
        candidates: &[Column],
    ) -> Option<String>;
}

/// Order newly-created tables so a table appears after every other new table it
/// references by foreign key — Postgres checks an inline `references` at table
/// creation, so the target must already exist.
///
/// A stable topological sort over the create-set only (FKs to pre-existing tables,
/// or to tables outside this set, impose no ordering). The input order (callers
/// pass tables sorted by name) is preserved as the tie-break, and any FK cycle or
/// self-reference falls back to that order so output stays deterministic.
fn topo_sort_creates(tables: &[&Table]) -> Vec<Table> {
    let in_set: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
    let depends_on_unplaced = |table: &Table, placed: &[String]| -> bool {
        table.columns.iter().any(|column| {
            column.references.as_ref().is_some_and(|fk| {
                fk.table != table.name
                    && in_set.contains(&fk.table.as_str())
                    && !placed.iter().any(|name| name == &fk.table)
            })
        })
    };

    let mut placed: Vec<String> = Vec::with_capacity(tables.len());
    let mut ordered: Vec<Table> = Vec::with_capacity(tables.len());
    let mut remaining: Vec<&Table> = tables.to_vec();
    while !remaining.is_empty() {
        // First table whose every in-set FK target is already placed.
        let next = remaining
            .iter()
            .position(|table| !depends_on_unplaced(table, &placed));
        // A cycle leaves nothing placeable: emit the next in input order to make
        // progress (Postgres would reject the cycle anyway; staying deterministic
        // beats looping forever).
        let index = next.unwrap_or(0);
        let table = remaining.remove(index);
        placed.push(table.name.clone());
        ordered.push(table.clone());
    }
    ordered
}

/// Diff `old` → `new`, producing ordered changes.
///
/// Ordering is chosen so the emitted SQL applies cleanly and its reverse (see
/// [`down_sql`]) does too: roles are **created/altered first** (so grants and
/// policies can reference them) and **dropped last** (after any revokes), tables
/// are created in FK-dependency order, and a created/dropped table carries its own
/// indexes, grants, RLS state, and policies (see [`create_object_sql`]).
pub fn diff(old: &Schema, new: &Schema, resolver: &mut dyn Resolver) -> Vec<Change> {
    let mut changes = Vec::new();

    // Roles first: a grant or policy may reference a role, so it must exist before them.
    for role in &new.roles {
        match old.role(&role.name) {
            None => changes.push(Change::CreateRole(role.clone())),
            Some(previous) if previous != role => changes.push(Change::AlterRole {
                from: previous.clone(),
                to: role.clone(),
            }),
            Some(_) => {}
        }
    }

    // Drop removed tables in the **reverse** of create order: a table must be dropped
    // before any table it references by FK exists no more — i.e. children before parents.
    // `topo_sort_creates` orders parents-before-children, so reverse it. (`down_sql`
    // reverses the whole change list again, so the recreated tables come back
    // parents-first, satisfying their inline `references`.)
    let to_drop: Vec<&Table> = old
        .tables
        .iter()
        .filter(|table| new.table(&table.name).is_none())
        .collect();
    let mut dropped = topo_sort_creates(&to_drop);
    dropped.reverse();
    for table in dropped {
        changes.push(Change::DropTable(table));
    }
    // Tables that don't exist yet are created in FK-dependency order; existing
    // tables are diffed column-by-column (order among those is irrelevant).
    let to_create: Vec<&Table> = new
        .tables
        .iter()
        .filter(|table| old.table(&table.name).is_none())
        .collect();
    for table in topo_sort_creates(&to_create) {
        changes.push(Change::CreateTable(table));
    }
    for new_table in &new.tables {
        if let Some(old_table) = old.table(&new_table.name) {
            diff_table(old_table, new_table, resolver, &mut changes);
            diff_table_rls(old_table, new_table, &mut changes);
            diff_table_indexes(old_table, new_table, &mut changes);
        }
    }

    // Roles last: a role can only be dropped once its grants/policies are gone
    // (those revokes are emitted above, for the tables that dropped them).
    for role in &old.roles {
        if new.role(&role.name).is_none() {
            changes.push(Change::DropRole(role.clone()));
        }
    }

    changes
}

/// Human-readable warnings for changes that **widen** access (a security downgrade),
/// so the transition isn't buried in the generated SQL. For the `gen` command to
/// print; non-fatal.
#[must_use]
pub fn widening_warnings(changes: &[Change]) -> Vec<String> {
    let mut warnings = Vec::new();
    for change in changes {
        match change {
            Change::SetRls {
                table,
                enabled: false,
            } => warnings.push(format!(
                "disables row-level security on \"{table}\" — every row becomes visible to \
                 anyone with table access"
            )),
            Change::SetForceRls {
                table,
                forced: false,
            } => warnings.push(format!(
                "un-forces row-level security on \"{table}\" — the table owner now bypasses \
                 all policies"
            )),
            _ => {}
        }
    }
    warnings
}

/// Diff the row-level-security aspects of an **existing** table (RLS enable/force,
/// grants, policies). A newly created or dropped table carries these inside its
/// [`Change::CreateTable`]/[`Change::DropTable`] (rendered by [`create_object_sql`]),
/// so this only runs for tables present in both schemas.
fn diff_table_rls(old: &Table, new: &Table, changes: &mut Vec<Change>) {
    if old.rls != new.rls {
        changes.push(Change::SetRls {
            table: new.name.clone(),
            enabled: new.rls,
        });
    }
    if old.force_rls != new.force_rls {
        changes.push(Change::SetForceRls {
            table: new.name.clone(),
            forced: new.force_rls,
        });
    }
    diff_grants(old, new, changes);
    diff_policies(old, new, changes);
}

/// Diff grants by role. A role whose privilege set changes is **fully replaced**
/// (revoke the old set, then grant the new) — bulletproof against the `ALL`
/// pseudo-privilege overlapping the specific ones, at the cost of a little churn.
fn diff_grants(old: &Table, new: &Table, changes: &mut Vec<Change>) {
    for grant in &new.grants {
        let new_privileges = canonical_privileges(&grant.privileges);
        match old.grant(&grant.role) {
            None => changes.push(Change::Grant {
                table: new.name.clone(),
                role: grant.role.clone(),
                privileges: new_privileges,
            }),
            Some(previous) => {
                let old_privileges = canonical_privileges(&previous.privileges);
                if old_privileges != new_privileges {
                    changes.push(Change::Revoke {
                        table: new.name.clone(),
                        role: grant.role.clone(),
                        privileges: old_privileges,
                    });
                    changes.push(Change::Grant {
                        table: new.name.clone(),
                        role: grant.role.clone(),
                        privileges: new_privileges,
                    });
                }
            }
        }
    }
    for grant in &old.grants {
        if new.grant(&grant.role).is_none() {
            changes.push(Change::Revoke {
                table: new.name.clone(),
                role: grant.role.clone(),
                privileges: canonical_privileges(&grant.privileges),
            });
        }
    }
}

/// Diff policies by name. Drops (and the old half of a change) come before creates
/// so a changed policy is dropped before its replacement is created (same name).
fn diff_policies(old: &Table, new: &Table, changes: &mut Vec<Change>) {
    for policy in &old.policies {
        let gone_or_changed = new
            .policy(&policy.name)
            .is_none_or(|current| current != policy);
        if gone_or_changed {
            changes.push(Change::DropPolicy {
                table: new.name.clone(),
                policy: policy.clone(),
            });
        }
    }
    for policy in &new.policies {
        let new_or_changed = old
            .policy(&policy.name)
            .is_none_or(|previous| previous != policy);
        if new_or_changed {
            changes.push(Change::CreatePolicy {
                table: new.name.clone(),
                policy: policy.clone(),
            });
        }
    }
}

fn diff_table(old: &Table, new: &Table, resolver: &mut dyn Resolver, changes: &mut Vec<Change>) {
    for new_column in &new.columns {
        if let Some(old_column) = old.column(&new_column.name) {
            if new_column.type_differs(old_column) {
                changes.push(Change::AlterType {
                    table: new.name.clone(),
                    from: old_column.clone(),
                    to: new_column.clone(),
                });
            }
        }
    }

    let mut removed: Vec<Column> = old
        .columns
        .iter()
        .filter(|c| new.column(&c.name).is_none())
        .cloned()
        .collect();
    let added: Vec<Column> = new
        .columns
        .iter()
        .filter(|c| old.column(&c.name).is_none())
        .cloned()
        .collect();

    for column in added {
        let target = if removed.is_empty() {
            None
        } else {
            resolver.rename_target(&new.name, &column, &removed)
        };
        match target.and_then(|name| removed.iter().position(|c| c.name == name)) {
            Some(index) => {
                let old_column = removed.remove(index);
                changes.push(Change::RenameColumn {
                    table: new.name.clone(),
                    from: old_column.name.clone(),
                    to: column.name.clone(),
                    to_def: column.clone(),
                    from_def: old_column.clone(),
                });
                if old_column.type_differs(&column) {
                    changes.push(Change::AlterType {
                        table: new.name.clone(),
                        from: rename_keep_type(&old_column, &column.name),
                        to: column,
                    });
                }
            }
            None => changes.push(Change::AddColumn {
                table: new.name.clone(),
                column,
            }),
        }
    }

    for column in removed {
        changes.push(Change::DropColumn {
            table: new.name.clone(),
            column,
        });
    }
}

/// The old column under its new (renamed) name, so an `AlterType` after a rename
/// targets the right column.
fn rename_keep_type(old: &Column, new_name: &str) -> Column {
    let mut column = old.clone();
    new_name.clone_into(&mut column.name);
    column
}

/// Quote a SQL identifier (double-quote, doubling embedded quotes).
fn quote(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    out.push('"');
    for character in name.chars() {
        if character == '"' {
            out.push('"');
        }
        out.push(character);
    }
    out.push('"');
    out
}

fn column_ddl(column: &Column) -> String {
    let mut ddl = format!("{} {}", quote(&column.name), column.sql_type);
    if !column.nullable {
        ddl.push_str(" not null");
    }
    if column.unique {
        ddl.push_str(" unique");
    }
    if let Some(expr) = &column.generated {
        // A stored generated column (e.g. a tsvector) — the database computes it; it
        // is mutually exclusive with a default.
        let _ = write!(ddl, " generated always as ({expr}) stored");
    } else if let Some(default) = &column.default {
        let _ = write!(ddl, " default {default}");
    }
    if let Some(fk) = &column.references {
        let _ = write!(
            ddl,
            " references {} ({}) on delete {}",
            quote(&fk.table),
            quote(&fk.column),
            fk.on_delete
        );
    }
    ddl
}

/// Conventional index name for a single-column index.
fn index_name(table: &str, column: &str) -> String {
    format!("idx_{table}_{column}")
}

/// `create index "idx_t_c" on "t" using <method> ("c" <opclass>);` — the `using` and
/// operator-class clauses are dropped when not set (the portable B-tree default).
fn create_index_sql(table: &str, column: &Column) -> String {
    let using = column
        .index_method
        .as_deref()
        .map_or_else(String::new, |method| format!(" using {method}"));
    let opclass = column
        .opclass
        .as_deref()
        .map_or_else(String::new, |opclass| format!(" {opclass}"));
    format!(
        "create index {} on {}{using} ({}{opclass});",
        quote(&index_name(table, &column.name)),
        quote(table),
        quote(&column.name)
    )
}

/// `create [unique] index "<name>" on "<table>" [using <method>] ("c1","c2",…) [where <pred>];`
/// for a table-level [`Index`]. Identifiers are quoted; `method` is a validated bare
/// identifier; `predicate` is verbatim trusted SQL (reviewed before apply).
fn create_table_index_sql(table: &str, index: &Index) -> String {
    let unique = if index.unique { "unique " } else { "" };
    let using = index
        .method
        .as_deref()
        .map_or_else(String::new, |method| format!(" using {method}"));
    let columns = index
        .columns
        .iter()
        .map(|column| quote(column))
        .collect::<Vec<_>>()
        .join(", ");
    let mut sql = format!(
        "create {unique}index {} on {}{using} ({columns})",
        quote(&index.name),
        quote(table)
    );
    if let Some(predicate) = &index.predicate {
        let _ = write!(sql, " where {predicate}");
    }
    sql.push(';');
    sql
}

/// `drop index "<name>";` — Postgres index names are schema-scoped (no table qualifier).
fn drop_index_sql(index: &Index) -> String {
    format!("drop index {};", quote(&index.name))
}

/// Diff table-level indexes by name. A changed index (any field differs) is dropped
/// and recreated under the same name; drops are emitted before creates.
fn diff_table_indexes(old: &Table, new: &Table, changes: &mut Vec<Change>) {
    for index in &old.indexes {
        let gone_or_changed = new
            .index(&index.name)
            .is_none_or(|current| current != index);
        if gone_or_changed {
            changes.push(Change::DropIndex {
                table: new.name.clone(),
                index: index.clone(),
            });
        }
    }
    for index in &new.indexes {
        let new_or_changed = old
            .index(&index.name)
            .is_none_or(|previous| previous != index);
        if new_or_changed {
            changes.push(Change::CreateIndex {
                table: new.name.clone(),
                index: index.clone(),
            });
        }
    }
}

fn create_table_sql(table: &Table) -> String {
    let mut lines: Vec<String> = table
        .columns
        .iter()
        .map(|c| format!("    {}", column_ddl(c)))
        .collect();
    let pk: Vec<String> = table
        .columns
        .iter()
        .filter(|c| c.pk)
        .map(|c| quote(&c.name))
        .collect();
    if !pk.is_empty() {
        lines.push(format!("    primary key ({})", pk.join(", ")));
    }
    format!(
        "create table {} (\n{}\n);",
        quote(&table.name),
        lines.join(",\n")
    )
}

fn alter_type_sql(table: &str, to: &Column) -> String {
    let name = quote(&to.name);
    let mut sql = format!(
        "alter table {} alter column {} type {} using {}::{};",
        quote(table),
        name,
        to.sql_type,
        name,
        to.sql_type
    );
    sql.push('\n');
    let _ = write!(
        sql,
        "alter table {} alter column {} {} not null;",
        quote(table),
        name,
        if to.nullable { "drop" } else { "set" }
    );
    sql
}

/// Canonical privilege list for stable output and comparison: `ALL` collapses to a
/// lone `All` (it subsumes the rest), otherwise the set is sorted and deduplicated.
fn canonical_privileges(privileges: &[Privilege]) -> Vec<Privilege> {
    if privileges.contains(&Privilege::All) {
        return vec![Privilege::All];
    }
    let mut sorted: Vec<Privilege> = privileges.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    sorted
}

/// Comma-joined privilege keywords for a `GRANT`/`REVOKE` (`all privileges` when `ALL`).
fn privilege_list(privileges: &[Privilege]) -> String {
    canonical_privileges(privileges)
        .iter()
        .map(|privilege| privilege.as_sql())
        .collect::<Vec<_>>()
        .join(", ")
}

fn create_role_sql(role: &Role) -> String {
    let mut sql = format!("create role {}", quote(&role.name));
    // Only positive attributes are emitted; `CREATE ROLE` already defaults to
    // NOLOGIN/NOCREATEDB/NOCREATEROLE/NOBYPASSRLS.
    if role.login {
        sql.push_str(" login");
    }
    if role.createdb {
        sql.push_str(" createdb");
    }
    if role.createrole {
        sql.push_str(" createrole");
    }
    if role.bypassrls {
        sql.push_str(" bypassrls");
    }
    sql.push(';');
    sql
}

/// `ALTER ROLE` spelling every attribute explicitly (positive or negative) so the
/// statement fully defines the role's state and is exactly reversible.
fn alter_role_sql(role: &Role) -> String {
    format!(
        "alter role {} with {} {} {} {};",
        quote(&role.name),
        if role.login { "login" } else { "nologin" },
        if role.createdb {
            "createdb"
        } else {
            "nocreatedb"
        },
        if role.createrole {
            "createrole"
        } else {
            "nocreaterole"
        },
        if role.bypassrls {
            "bypassrls"
        } else {
            "nobypassrls"
        },
    )
}

fn set_rls_sql(table: &str, enabled: bool) -> String {
    format!(
        "alter table {} {} row level security;",
        quote(table),
        if enabled { "enable" } else { "disable" }
    )
}

fn force_rls_sql(table: &str, forced: bool) -> String {
    format!(
        "alter table {} {} row level security;",
        quote(table),
        if forced { "force" } else { "no force" }
    )
}

fn grant_sql(table: &str, role: &str, privileges: &[Privilege]) -> String {
    format!(
        "grant {} on {} to {};",
        privilege_list(privileges),
        quote(table),
        quote(role)
    )
}

fn revoke_sql(table: &str, role: &str, privileges: &[Privilege]) -> String {
    format!(
        "revoke {} on {} from {};",
        privilege_list(privileges),
        quote(table),
        quote(role)
    )
}

/// `CREATE POLICY … FOR <command> [TO roles] [USING (expr)] [WITH CHECK (expr)]`.
/// Identifiers are quoted; the `using`/`check` expressions are developer-supplied
/// trusted SQL written verbatim (the review-before-apply gate is the control, §17).
fn create_policy_sql(table: &str, policy: &Policy) -> String {
    let mut sql = format!(
        "create policy {} on {} for {}",
        quote(&policy.name),
        quote(table),
        policy.command.as_sql()
    );
    if !policy.roles.is_empty() {
        let roles: Vec<String> = policy.roles.iter().map(|role| quote(role)).collect();
        let _ = write!(sql, " to {}", roles.join(", "));
    }
    if let Some(using) = &policy.using {
        let _ = write!(sql, " using ({using})");
    }
    if let Some(check) = &policy.check {
        let _ = write!(sql, " with check ({check})");
    }
    sql.push(';');
    sql
}

fn drop_policy_sql(table: &str, policy: &Policy) -> String {
    format!("drop policy {} on {};", quote(&policy.name), quote(table))
}

/// Full DDL to create a table object: the table, its secondary indexes, grants, RLS
/// enable/force, and policies — in apply order. Used by both `up(CreateTable)` and
/// `down(DropTable)` so creating and reverting a table are exact mirrors (a plain
/// `drop table` cascades all of these away).
fn create_object_sql(table: &Table) -> String {
    let mut sql = create_table_sql(table);
    for column in table.columns.iter().filter(|c| c.index) {
        sql.push('\n');
        sql.push_str(&create_index_sql(&table.name, column));
    }
    // Table-level indexes after the table (and its single-column indexes); the
    // referenced columns — including a generated `tsvector` for a GIN index — already
    // exist by now since they are part of the CREATE TABLE above.
    for index in &table.indexes {
        sql.push('\n');
        sql.push_str(&create_table_index_sql(&table.name, index));
    }
    for grant in &table.grants {
        sql.push('\n');
        sql.push_str(&grant_sql(&table.name, &grant.role, &grant.privileges));
    }
    if table.rls {
        sql.push('\n');
        sql.push_str(&set_rls_sql(&table.name, true));
    }
    if table.force_rls {
        sql.push('\n');
        sql.push_str(&force_rls_sql(&table.name, true));
    }
    for policy in &table.policies {
        sql.push('\n');
        sql.push_str(&create_policy_sql(&table.name, policy));
    }
    sql
}

/// Render the forward (up) SQL for a change.
fn up(change: &Change) -> String {
    match change {
        Change::CreateTable(table) => create_object_sql(table),
        Change::DropTable(table) => format!("drop table {};", quote(&table.name)),
        Change::AddColumn { table, column } => {
            let mut sql = format!(
                "alter table {} add column {};",
                quote(table),
                column_ddl(column)
            );
            if column.index {
                sql.push('\n');
                sql.push_str(&create_index_sql(table, column));
            }
            sql
        }
        Change::DropColumn { table, column } => {
            format!(
                "alter table {} drop column {};",
                quote(table),
                quote(&column.name)
            )
        }
        Change::AlterType { table, to, .. } => alter_type_sql(table, to),
        Change::RenameColumn {
            table, from, to, ..
        } => {
            format!(
                "alter table {} rename column {} to {};",
                quote(table),
                quote(from),
                quote(to)
            )
        }
        Change::CreateRole(role) => create_role_sql(role),
        Change::DropRole(role) => format!("drop role {};", quote(&role.name)),
        Change::AlterRole { to, .. } => alter_role_sql(to),
        Change::SetRls { table, enabled } => set_rls_sql(table, *enabled),
        Change::SetForceRls { table, forced } => force_rls_sql(table, *forced),
        Change::Grant {
            table,
            role,
            privileges,
        } => grant_sql(table, role, privileges),
        Change::Revoke {
            table,
            role,
            privileges,
        } => revoke_sql(table, role, privileges),
        Change::CreatePolicy { table, policy } => create_policy_sql(table, policy),
        Change::DropPolicy { table, policy } => drop_policy_sql(table, policy),
        Change::CreateIndex { table, index } => create_table_index_sql(table, index),
        Change::DropIndex { index, .. } => drop_index_sql(index),
    }
}

/// Render the reverse (down) SQL for a change.
fn down(change: &Change) -> String {
    match change {
        Change::CreateTable(table) => format!("drop table {};", quote(&table.name)),
        Change::DropTable(table) => create_object_sql(table),
        Change::AddColumn { table, column } => {
            format!(
                "alter table {} drop column {};",
                quote(table),
                quote(&column.name)
            )
        }
        Change::DropColumn { table, column } => {
            // Mirror `up(AddColumn)`: re-add the column *and* recreate its index, so
            // dropping an indexed column is faithfully reversible.
            let mut sql = format!(
                "alter table {} add column {};",
                quote(table),
                column_ddl(column)
            );
            if column.index {
                sql.push('\n');
                sql.push_str(&create_index_sql(table, column));
            }
            sql
        }
        Change::AlterType { table, from, .. } => alter_type_sql(table, from),
        Change::RenameColumn {
            table, from, to, ..
        } => {
            format!(
                "alter table {} rename column {} to {};",
                quote(table),
                quote(to),
                quote(from)
            )
        }
        // A created role is dropped; a dropped role is recreated.
        Change::CreateRole(role) => format!("drop role {};", quote(&role.name)),
        Change::DropRole(role) => create_role_sql(role),
        Change::AlterRole { from, .. } => alter_role_sql(from),
        // RLS/force toggles invert: undoing an enable disables, and vice versa.
        Change::SetRls { table, enabled } => set_rls_sql(table, !*enabled),
        Change::SetForceRls { table, forced } => force_rls_sql(table, !*forced),
        // A grant is undone by a revoke (and vice versa) of the same privileges.
        Change::Grant {
            table,
            role,
            privileges,
        } => revoke_sql(table, role, privileges),
        Change::Revoke {
            table,
            role,
            privileges,
        } => grant_sql(table, role, privileges),
        Change::CreatePolicy { table, policy } => drop_policy_sql(table, policy),
        Change::DropPolicy { table, policy } => create_policy_sql(table, policy),
        // An index is undone by dropping it; a drop is undone by recreating it.
        Change::CreateIndex { index, .. } => drop_index_sql(index),
        Change::DropIndex { table, index } => create_table_index_sql(table, index),
    }
}

/// Build the up migration SQL from ordered changes.
#[must_use]
pub fn up_sql(changes: &[Change]) -> String {
    changes.iter().map(up).collect::<Vec<_>>().join("\n")
}

/// Build the down migration SQL (inverse changes in reverse order).
#[must_use]
pub fn down_sql(changes: &[Change]) -> String {
    changes
        .iter()
        .rev()
        .map(down)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::{Change, Resolver, diff, down_sql, up_sql, widening_warnings};
    use crate::model::{
        Column, Grant, Index, Policy, PolicyCommand, Privilege, Role, Schema, Table,
    };

    fn index(name: &str, columns: &[&str]) -> Index {
        Index {
            name: name.to_owned(),
            columns: columns.iter().map(|c| (*c).to_owned()).collect(),
            unique: false,
            method: None,
            predicate: None,
        }
    }

    fn table_with_indexes(name: &str, columns: Vec<Column>, indexes: Vec<Index>) -> Schema {
        Schema {
            tables: vec![Table {
                name: name.to_owned(),
                columns,
                indexes,
                ..Table::default()
            }],
            roles: Vec::new(),
        }
    }

    fn col(name: &str, ty: &str) -> Column {
        Column {
            name: name.to_owned(),
            sql_type: ty.to_owned(),
            nullable: false,
            pk: name == "id",
            unique: false,
            index: false,
            index_method: None,
            opclass: None,
            generated: None,
            default: None,
            references: None,
        }
    }

    fn table(name: &str, columns: Vec<Column>) -> Schema {
        Schema {
            tables: vec![Table {
                name: name.to_owned(),
                columns,
                ..Table::default()
            }],
            ..Schema::default()
        }
    }

    struct NeverRename;
    impl Resolver for NeverRename {
        fn rename_target(&mut self, _: &str, _: &Column, _: &[Column]) -> Option<String> {
            None
        }
    }

    struct AlwaysRename;
    impl Resolver for AlwaysRename {
        fn rename_target(&mut self, _: &str, _: &Column, candidates: &[Column]) -> Option<String> {
            candidates.first().map(|c| c.name.clone())
        }
    }

    fn indexed_col(name: &str, ty: &str) -> Column {
        let mut column = col(name, ty);
        column.index = true;
        column
    }

    #[test]
    fn create_index_emits_method_and_opclass() {
        let mut embedding = indexed_col("embedding", "vector(3)");
        embedding.index_method = Some("hnsw".to_owned());
        embedding.opclass = Some("vector_cosine_ops".to_owned());
        let new = table("docs", vec![col("id", "bigint"), embedding]);
        let up = up_sql(&diff(&Schema::default(), &new, &mut NeverRename));
        assert!(
            up.contains(
                r#"create index "idx_docs_embedding" on "docs" using hnsw ("embedding" vector_cosine_ops);"#
            ),
            "missing hnsw/opclass index, got:\n{up}"
        );
    }

    #[test]
    fn create_index_emits_gin_method_without_opclass() {
        let mut tsv = indexed_col("body_tsv", "tsvector");
        tsv.index_method = Some("gin".to_owned());
        let new = table("articles", vec![col("id", "bigint"), tsv]);
        let up = up_sql(&diff(&Schema::default(), &new, &mut NeverRename));
        assert!(
            up.contains(
                r#"create index "idx_articles_body_tsv" on "articles" using gin ("body_tsv");"#
            ),
            "missing gin index, got:\n{up}"
        );
    }

    #[test]
    fn generated_tsvector_column_emits_stored_clause_and_gin_index() {
        let mut tsv = indexed_col("body_tsv", "tsvector");
        tsv.index_method = Some("gin".to_owned());
        tsv.generated = Some("to_tsvector('english', body)".to_owned());
        let new = table(
            "articles",
            vec![col("id", "bigint"), col("body", "text"), tsv],
        );
        let up = up_sql(&diff(&Schema::default(), &new, &mut NeverRename));
        assert!(
            up.contains(
                r#""body_tsv" tsvector not null generated always as (to_tsvector('english', body)) stored"#
            ),
            "missing generated stored column, got:\n{up}"
        );
        assert!(
            up.contains(
                r#"create index "idx_articles_body_tsv" on "articles" using gin ("body_tsv");"#
            ),
            "missing gin index, got:\n{up}"
        );
    }

    #[test]
    fn bare_index_has_no_using_clause() {
        let new = table(
            "logs",
            vec![col("id", "bigint"), indexed_col("user_id", "bigint")],
        );
        let up = up_sql(&diff(&Schema::default(), &new, &mut NeverRename));
        assert!(up.contains(r#"create index "idx_logs_user_id" on "logs" ("user_id");"#));
        assert!(
            !up.contains("using"),
            "bare index must be B-tree, got:\n{up}"
        );
    }

    #[test]
    fn create_table_emits_create_index_for_indexed_columns() {
        let new = table(
            "logs",
            vec![col("id", "bigint"), indexed_col("user_id", "bigint")],
        );
        let up = up_sql(&diff(&Schema::default(), &new, &mut NeverRename));
        assert!(up.contains("create table \"logs\""));
        assert!(
            up.contains("create index \"idx_logs_user_id\" on \"logs\" (\"user_id\");"),
            "missing create index, got:\n{up}"
        );
    }

    #[test]
    fn add_indexed_column_emits_create_index() {
        let old = table("logs", vec![col("id", "bigint")]);
        let new = table(
            "logs",
            vec![col("id", "bigint"), indexed_col("user_id", "bigint")],
        );
        let up = up_sql(&diff(&old, &new, &mut NeverRename));
        assert!(up.contains("add column \"user_id\""));
        assert!(up.contains("create index \"idx_logs_user_id\" on \"logs\" (\"user_id\");"));
    }

    #[test]
    fn create_table_then_drop() {
        let new = table("users", vec![col("id", "uuid"), col("email", "text")]);
        let changes = diff(&Schema::default(), &new, &mut NeverRename);
        assert!(matches!(changes.as_slice(), [Change::CreateTable(_)]));
        let up = up_sql(&changes);
        assert!(up.contains("create table \"users\""));
        assert!(up.contains("primary key (\"id\")"));
        assert_eq!(down_sql(&changes), "drop table \"users\";");
    }

    fn pk_col(name: &str, ty: &str) -> Column {
        let mut column = col(name, ty);
        column.pk = true;
        column
    }

    #[test]
    fn composite_primary_key_emits_combined_pk_clause() {
        let new = table(
            "goal_step",
            vec![
                pk_col("goal_id", "uuid"),
                pk_col("step_id", "uuid"),
                col("position", "int"),
            ],
        );
        let up = up_sql(&diff(&Schema::default(), &new, &mut NeverRename));
        assert!(
            up.contains(r#"primary key ("goal_id", "step_id")"#),
            "missing composite primary key, got:\n{up}"
        );
    }

    #[test]
    fn add_column_emits_alter_add() {
        let old = table("users", vec![col("id", "uuid")]);
        let new = table("users", vec![col("id", "uuid"), col("email", "text")]);
        let changes = diff(&old, &new, &mut NeverRename);
        assert_eq!(
            up_sql(&changes),
            "alter table \"users\" add column \"email\" text not null;"
        );
        assert_eq!(
            down_sql(&changes),
            "alter table \"users\" drop column \"email\";"
        );
    }

    #[test]
    fn drop_and_add_without_rename() {
        let old = table("users", vec![col("id", "uuid"), col("old", "text")]);
        let new = table("users", vec![col("id", "uuid"), col("new", "text")]);
        let changes = diff(&old, &new, &mut NeverRename);
        let up = up_sql(&changes);
        assert!(up.contains("add column \"new\""));
        assert!(up.contains("drop column \"old\""));
    }

    #[test]
    fn rename_when_resolver_says_so() {
        let old = table("users", vec![col("id", "uuid"), col("old", "text")]);
        let new = table("users", vec![col("id", "uuid"), col("new", "text")]);
        let changes = diff(&old, &new, &mut AlwaysRename);
        assert_eq!(
            up_sql(&changes),
            "alter table \"users\" rename column \"old\" to \"new\";"
        );
        assert_eq!(
            down_sql(&changes),
            "alter table \"users\" rename column \"new\" to \"old\";"
        );
    }

    #[test]
    fn type_change_emits_alter_type() {
        let old = table("users", vec![col("id", "uuid"), col("age", "int")]);
        let new = table("users", vec![col("id", "uuid"), col("age", "bigint")]);
        let changes = diff(&old, &new, &mut NeverRename);
        let up = up_sql(&changes);
        assert!(up.contains("alter column \"age\" type bigint"), "{up}");
    }

    fn fk_col(name: &str, target_table: &str) -> Column {
        let mut column = col(name, "uuid");
        column.references = Some(crate::model::ForeignKey {
            table: target_table.to_owned(),
            column: "id".to_owned(),
            on_delete: "cascade".to_owned(),
        });
        column
    }

    #[test]
    fn create_tables_are_ordered_so_fk_targets_come_first() {
        // Alphabetically `account_invites` < `accounts`, but it references
        // `accounts`, so the referenced table must be created first.
        let new = Schema {
            tables: vec![
                Table {
                    name: "account_invites".to_owned(),
                    columns: vec![col("id", "uuid"), fk_col("account_id", "accounts")],
                    ..Table::default()
                },
                Table {
                    name: "accounts".to_owned(),
                    columns: vec![col("id", "uuid"), fk_col("owner_id", "users")],
                    ..Table::default()
                },
                Table {
                    name: "users".to_owned(),
                    columns: vec![col("id", "uuid")],
                    ..Table::default()
                },
            ],
            ..Schema::default()
        };
        let up = up_sql(&diff(&Schema::default(), &new, &mut NeverRename));
        let users_at = up.find("create table \"users\"").expect("users");
        let accounts_at = up.find("create table \"accounts\"").expect("accounts");
        let invites_at = up
            .find("create table \"account_invites\"")
            .expect("account_invites");
        assert!(users_at < accounts_at, "users must precede accounts:\n{up}");
        assert!(
            accounts_at < invites_at,
            "accounts must precede account_invites:\n{up}"
        );
    }

    #[test]
    fn fk_to_preexisting_table_does_not_reorder() {
        // `users` already exists; a new `posts` referencing it needs no reordering
        // and a self/external FK must not break the create.
        let old = table("users", vec![col("id", "uuid")]);
        let new = Schema {
            tables: vec![
                Table {
                    name: "posts".to_owned(),
                    columns: vec![col("id", "uuid"), fk_col("author_id", "users")],
                    ..Table::default()
                },
                Table {
                    name: "users".to_owned(),
                    columns: vec![col("id", "uuid")],
                    ..Table::default()
                },
            ],
            ..Schema::default()
        };
        let changes = diff(&old, &new, &mut NeverRename);
        // Only `posts` is created; `users` already exists.
        assert!(matches!(changes.as_slice(), [Change::CreateTable(t)] if t.name == "posts"));
    }

    #[test]
    fn drop_fk_tables_are_reverse_topo_ordered() {
        // `z_child` references `a_parent`; alphabetically a_parent < z_child, but the
        // child must be dropped first (and `down` must recreate the parent first).
        let old = Schema {
            tables: vec![
                Table {
                    name: "a_parent".to_owned(),
                    columns: vec![col("id", "uuid")],
                    ..Table::default()
                },
                Table {
                    name: "z_child".to_owned(),
                    columns: vec![col("id", "uuid"), fk_col("parent_id", "a_parent")],
                    ..Table::default()
                },
            ],
            ..Schema::default()
        };
        let changes = diff(&old, &Schema::default(), &mut NeverRename);
        let up = up_sql(&changes);
        let parent_at = up.find(r#"drop table "a_parent""#).expect("parent drop");
        let child_at = up.find(r#"drop table "z_child""#).expect("child drop");
        assert!(child_at < parent_at, "child must drop before parent:\n{up}");
        let down = down_sql(&changes);
        let parent_create = down
            .find(r#"create table "a_parent""#)
            .expect("parent recreate");
        let child_create = down
            .find(r#"create table "z_child""#)
            .expect("child recreate");
        assert!(
            parent_create < child_create,
            "down must recreate the parent before the child:\n{down}"
        );
    }

    #[test]
    fn drop_indexed_column_down_restores_index() {
        let old = table(
            "logs",
            vec![col("id", "bigint"), indexed_col("user_id", "bigint")],
        );
        let new = table("logs", vec![col("id", "bigint")]);
        let changes = diff(&old, &new, &mut NeverRename);
        let down = down_sql(&changes);
        assert!(
            down.contains(r#"add column "user_id""#),
            "down re-adds the column:\n{down}"
        );
        assert!(
            down.contains(r#"create index "idx_logs_user_id" on "logs" ("user_id");"#),
            "down must recreate the dropped column's index:\n{down}"
        );
    }

    // ---- Row-level security: roles, RLS toggles, grants, policies ----

    fn role(name: &str) -> Role {
        Role {
            name: name.to_owned(),
            login: false,
            createdb: false,
            createrole: false,
            bypassrls: false,
        }
    }

    fn select_policy(name: &str, role: &str, using: &str) -> Policy {
        Policy {
            name: name.to_owned(),
            command: PolicyCommand::Select,
            roles: vec![role.to_owned()],
            using: Some(using.to_owned()),
            check: None,
        }
    }

    fn schema_with(tables: Vec<Table>, roles: Vec<Role>) -> Schema {
        Schema { tables, roles }
    }

    #[test]
    fn create_role_emits_positive_attributes_only() {
        let mut app = role("app_user");
        app.login = true;
        app.bypassrls = true;
        let new = schema_with(Vec::new(), vec![app]);
        let changes = diff(&Schema::default(), &new, &mut NeverRename);
        assert_eq!(
            up_sql(&changes),
            r#"create role "app_user" login bypassrls;"#
        );
        assert_eq!(down_sql(&changes), r#"drop role "app_user";"#);
    }

    #[test]
    fn role_with_no_attributes_is_plain_create() {
        let new = schema_with(Vec::new(), vec![role("readonly")]);
        let up = up_sql(&diff(&Schema::default(), &new, &mut NeverRename));
        assert_eq!(up, r#"create role "readonly";"#);
    }

    #[test]
    fn role_is_created_before_a_table_grant_that_references_it() {
        let posts = Table {
            name: "posts".to_owned(),
            columns: vec![col("id", "uuid")],
            grants: vec![Grant {
                role: "app_user".to_owned(),
                privileges: vec![Privilege::Select],
            }],
            ..Table::default()
        };
        let new = schema_with(vec![posts], vec![role("app_user")]);
        let up = up_sql(&diff(&Schema::default(), &new, &mut NeverRename));
        let role_at = up.find("create role").expect("create role");
        let grant_at = up.find("grant select").expect("grant");
        assert!(
            role_at < grant_at,
            "role must be created before grant:\n{up}"
        );
    }

    #[test]
    fn dropped_role_comes_after_the_revoke_that_frees_it() {
        // posts grants to app_user; removing both the grant and the role must revoke
        // before dropping the role (Postgres refuses to drop a role with privileges).
        let old = schema_with(
            vec![Table {
                name: "posts".to_owned(),
                columns: vec![col("id", "uuid")],
                grants: vec![Grant {
                    role: "app_user".to_owned(),
                    privileges: vec![Privilege::Select],
                }],
                ..Table::default()
            }],
            vec![role("app_user")],
        );
        let new = schema_with(
            vec![Table {
                name: "posts".to_owned(),
                columns: vec![col("id", "uuid")],
                ..Table::default()
            }],
            Vec::new(),
        );
        let up = up_sql(&diff(&old, &new, &mut NeverRename));
        let revoke_at = up.find("revoke select").expect("revoke");
        let drop_role_at = up.find("drop role").expect("drop role");
        assert!(
            revoke_at < drop_role_at,
            "revoke must precede drop role:\n{up}"
        );
    }

    #[test]
    fn create_table_emits_grant_then_enable_rls_then_policy_in_order() {
        let posts = Table {
            name: "posts".to_owned(),
            columns: vec![col("id", "uuid"), col("author_id", "uuid")],
            rls: true,
            grants: vec![Grant {
                role: "app_user".to_owned(),
                privileges: vec![Privilege::Select],
            }],
            policies: vec![select_policy(
                "posts_owner",
                "app_user",
                "author_id = current_setting('app.user_id')::uuid",
            )],
            ..Table::default()
        };
        let new = schema_with(vec![posts], Vec::new());
        let up = up_sql(&diff(&Schema::default(), &new, &mut NeverRename));
        let table_at = up.find("create table").expect("table");
        let grant_at = up.find("grant select").expect("grant");
        let rls_at = up.find("enable row level security").expect("enable rls");
        let policy_at = up.find("create policy").expect("policy");
        assert!(table_at < grant_at, "table before grant:\n{up}");
        assert!(grant_at < rls_at, "grant before enable rls:\n{up}");
        assert!(rls_at < policy_at, "enable rls before policy:\n{up}");
        assert!(
            up.contains(
                r#"create policy "posts_owner" on "posts" for select to "app_user" using (author_id = current_setting('app.user_id')::uuid);"#
            ),
            "policy DDL wrong:\n{up}"
        );
    }

    #[test]
    fn create_table_down_is_a_plain_drop_that_cascades() {
        let posts = Table {
            name: "posts".to_owned(),
            columns: vec![col("id", "uuid")],
            rls: true,
            policies: vec![select_policy("p", "app_user", "true")],
            ..Table::default()
        };
        let changes = diff(
            &Schema::default(),
            &schema_with(vec![posts], Vec::new()),
            &mut NeverRename,
        );
        assert_eq!(down_sql(&changes), r#"drop table "posts";"#);
    }

    #[test]
    fn drop_table_down_restores_the_full_object() {
        // Dropping an RLS table must, on revert, recreate the table *and* its grant,
        // RLS enable, and policy — `create_object_sql` makes drop/create exact mirrors.
        let posts = Table {
            name: "posts".to_owned(),
            columns: vec![col("id", "uuid")],
            rls: true,
            grants: vec![Grant {
                role: "app_user".to_owned(),
                privileges: vec![Privilege::Select],
            }],
            policies: vec![select_policy("posts_owner", "app_user", "true")],
            ..Table::default()
        };
        let old = schema_with(vec![posts], Vec::new());
        let changes = diff(&old, &Schema::default(), &mut NeverRename);
        assert_eq!(up_sql(&changes), r#"drop table "posts";"#);
        let down = down_sql(&changes);
        assert!(
            down.contains(r#"create table "posts""#),
            "down recreates table:\n{down}"
        );
        assert!(
            down.contains(r#"grant select on "posts" to "app_user";"#),
            "down regrants:\n{down}"
        );
        assert!(
            down.contains("enable row level security"),
            "down re-enables rls:\n{down}"
        );
        assert!(
            down.contains(r#"create policy "posts_owner""#),
            "down recreates policy:\n{down}"
        );
    }

    fn plain_table(name: &str) -> Table {
        Table {
            name: name.to_owned(),
            columns: vec![col("id", "uuid")],
            ..Table::default()
        }
    }

    #[test]
    fn enable_rls_on_existing_table_toggles_both_ways() {
        let old = schema_with(vec![plain_table("posts")], Vec::new());
        let mut on = plain_table("posts");
        on.rls = true;
        let new = schema_with(vec![on], Vec::new());
        let changes = diff(&old, &new, &mut NeverRename);
        assert_eq!(
            up_sql(&changes),
            r#"alter table "posts" enable row level security;"#
        );
        assert_eq!(
            down_sql(&changes),
            r#"alter table "posts" disable row level security;"#
        );
    }

    #[test]
    fn force_rls_on_existing_table_toggles_both_ways() {
        let mut old_table = plain_table("posts");
        old_table.rls = true;
        let mut new_table = plain_table("posts");
        new_table.rls = true;
        new_table.force_rls = true;
        let changes = diff(
            &schema_with(vec![old_table], Vec::new()),
            &schema_with(vec![new_table], Vec::new()),
            &mut NeverRename,
        );
        assert_eq!(
            up_sql(&changes),
            r#"alter table "posts" force row level security;"#
        );
        assert_eq!(
            down_sql(&changes),
            r#"alter table "posts" no force row level security;"#
        );
    }

    #[test]
    fn add_policy_to_existing_table() {
        let mut old_table = plain_table("posts");
        old_table.rls = true;
        let mut new_table = plain_table("posts");
        new_table.rls = true;
        new_table.policies = vec![select_policy("p", "app_user", "true")];
        let changes = diff(
            &schema_with(vec![old_table], Vec::new()),
            &schema_with(vec![new_table], Vec::new()),
            &mut NeverRename,
        );
        assert_eq!(
            up_sql(&changes),
            r#"create policy "p" on "posts" for select to "app_user" using (true);"#
        );
        assert_eq!(down_sql(&changes), r#"drop policy "p" on "posts";"#);
    }

    #[test]
    fn changing_a_policy_drops_then_recreates_it() {
        let mut old_table = plain_table("posts");
        old_table.rls = true;
        old_table.policies = vec![select_policy("p", "app_user", "old_expr")];
        let mut new_table = plain_table("posts");
        new_table.rls = true;
        new_table.policies = vec![select_policy("p", "app_user", "new_expr")];
        let changes = diff(
            &schema_with(vec![old_table], Vec::new()),
            &schema_with(vec![new_table], Vec::new()),
            &mut NeverRename,
        );
        let up = up_sql(&changes);
        let drop_at = up.find("drop policy").expect("drop");
        let create_at = up.find("create policy").expect("create");
        assert!(
            drop_at < create_at,
            "drop before create for a changed policy:\n{up}"
        );
        assert!(up.contains("using (new_expr)"), "new expr applied:\n{up}");
        // Reverting restores the old definition.
        assert!(down_sql(&changes).contains("using (old_expr)"));
    }

    #[test]
    fn insert_policy_renders_with_check_only() {
        let policy = Policy {
            name: "ins".to_owned(),
            command: PolicyCommand::Insert,
            roles: vec!["app_user".to_owned()],
            using: None,
            check: Some("author_id = current_user_id()".to_owned()),
        };
        let mut table = plain_table("posts");
        table.rls = true;
        table.policies = vec![policy];
        let up = up_sql(&diff(
            &Schema::default(),
            &schema_with(vec![table], Vec::new()),
            &mut NeverRename,
        ));
        assert!(
            up.contains(
                r#"create policy "ins" on "posts" for insert to "app_user" with check (author_id = current_user_id());"#
            ),
            "insert policy DDL wrong:\n{up}"
        );
    }

    #[test]
    fn public_policy_omits_the_to_clause() {
        let policy = Policy {
            name: "all_read".to_owned(),
            command: PolicyCommand::Select,
            roles: Vec::new(),
            using: Some("true".to_owned()),
            check: None,
        };
        let mut table = plain_table("posts");
        table.rls = true;
        table.policies = vec![policy];
        let up = up_sql(&diff(
            &Schema::default(),
            &schema_with(vec![table], Vec::new()),
            &mut NeverRename,
        ));
        assert!(
            up.contains(r#"create policy "all_read" on "posts" for select using (true);"#),
            "public policy must omit TO:\n{up}"
        );
    }

    #[test]
    fn policy_to_multiple_roles_lists_them() {
        let policy = Policy {
            name: "p".to_owned(),
            command: PolicyCommand::All,
            roles: vec!["a".to_owned(), "b".to_owned()],
            using: Some("true".to_owned()),
            check: None,
        };
        let mut table = plain_table("posts");
        table.rls = true;
        table.policies = vec![policy];
        let up = up_sql(&diff(
            &Schema::default(),
            &schema_with(vec![table], Vec::new()),
            &mut NeverRename,
        ));
        assert!(up.contains(r#"for all to "a", "b" using (true)"#), "{up}");
    }

    #[test]
    fn add_grant_to_existing_table() {
        let old = schema_with(vec![plain_table("posts")], Vec::new());
        let mut new_table = plain_table("posts");
        new_table.grants = vec![Grant {
            role: "app_user".to_owned(),
            privileges: vec![Privilege::Select, Privilege::Insert],
        }];
        let changes = diff(
            &old,
            &schema_with(vec![new_table], Vec::new()),
            &mut NeverRename,
        );
        assert_eq!(
            up_sql(&changes),
            r#"grant select, insert on "posts" to "app_user";"#
        );
        assert_eq!(
            down_sql(&changes),
            r#"revoke select, insert on "posts" from "app_user";"#
        );
    }

    #[test]
    fn all_privileges_grant_renders_all_privileges() {
        let mut new_table = plain_table("posts");
        new_table.grants = vec![Grant {
            role: "app_user".to_owned(),
            privileges: vec![Privilege::All],
        }];
        let up = up_sql(&diff(
            &schema_with(vec![plain_table("posts")], Vec::new()),
            &schema_with(vec![new_table], Vec::new()),
            &mut NeverRename,
        ));
        assert_eq!(up, r#"grant all privileges on "posts" to "app_user";"#);
    }

    #[test]
    fn changing_a_grant_revokes_then_grants() {
        let mut old_table = plain_table("posts");
        old_table.grants = vec![Grant {
            role: "app_user".to_owned(),
            privileges: vec![Privilege::Select],
        }];
        let mut new_table = plain_table("posts");
        new_table.grants = vec![Grant {
            role: "app_user".to_owned(),
            privileges: vec![Privilege::Select, Privilege::Update],
        }];
        let changes = diff(
            &schema_with(vec![old_table], Vec::new()),
            &schema_with(vec![new_table], Vec::new()),
            &mut NeverRename,
        );
        let up = up_sql(&changes);
        let revoke_at = up.find("revoke").expect("revoke");
        let grant_at = up.find("grant").expect("grant");
        assert!(revoke_at < grant_at, "revoke old before grant new:\n{up}");
        assert!(
            up.contains(r#"grant select, update on "posts" to "app_user";"#),
            "{up}"
        );
    }

    #[test]
    fn removing_a_grant_revokes_it() {
        let mut old_table = plain_table("posts");
        old_table.grants = vec![Grant {
            role: "app_user".to_owned(),
            privileges: vec![Privilege::Select],
        }];
        let changes = diff(
            &schema_with(vec![old_table], Vec::new()),
            &schema_with(vec![plain_table("posts")], Vec::new()),
            &mut NeverRename,
        );
        assert_eq!(
            up_sql(&changes),
            r#"revoke select on "posts" from "app_user";"#
        );
    }

    #[test]
    fn changing_role_attributes_emits_a_reversible_alter() {
        let old = schema_with(Vec::new(), vec![role("app_user")]);
        let mut with_login = role("app_user");
        with_login.login = true;
        let new = schema_with(Vec::new(), vec![with_login]);
        let changes = diff(&old, &new, &mut NeverRename);
        assert_eq!(
            up_sql(&changes),
            r#"alter role "app_user" with login nocreatedb nocreaterole nobypassrls;"#
        );
        assert_eq!(
            down_sql(&changes),
            r#"alter role "app_user" with nologin nocreatedb nocreaterole nobypassrls;"#
        );
    }

    #[test]
    fn unchanged_rls_table_produces_no_changes() {
        let mut table = plain_table("posts");
        table.rls = true;
        table.grants = vec![Grant {
            role: "app_user".to_owned(),
            privileges: vec![Privilege::Select],
        }];
        table.policies = vec![select_policy("p", "app_user", "true")];
        let schema = schema_with(vec![table], vec![role("app_user")]);
        assert!(diff(&schema, &schema, &mut NeverRename).is_empty());
    }

    #[test]
    fn create_table_emits_composite_index_after_the_table() {
        let new = table_with_indexes(
            "event",
            vec![col("id", "bigint"), col("a", "bigint"), col("b", "bigint")],
            vec![index("idx_ab", &["a", "b"])],
        );
        let up = up_sql(&diff(&Schema::default(), &new, &mut NeverRename));
        // The index appears after the CREATE TABLE, columns quoted in order.
        assert!(
            up.contains(r#"create index "idx_ab" on "event" ("a", "b");"#),
            "got: {up}"
        );
        let table_pos = up.find("create table").expect("table");
        let index_pos = up.find("create index").expect("index");
        assert!(index_pos > table_pos, "index must follow the table");
    }

    #[test]
    fn add_unique_method_index_to_existing_table_is_reversible() {
        let columns = vec![col("id", "bigint"), col("a", "bigint"), col("b", "bigint")];
        let old = table_with_indexes("event", columns.clone(), Vec::new());
        let new = table_with_indexes(
            "event",
            columns,
            vec![Index {
                name: "uq_ab".to_owned(),
                columns: vec!["a".to_owned(), "b".to_owned()],
                unique: true,
                method: Some("btree".to_owned()),
                predicate: Some("a is not null".to_owned()),
            }],
        );
        let changes = diff(&old, &new, &mut NeverRename);
        assert_eq!(
            up_sql(&changes),
            r#"create unique index "uq_ab" on "event" using btree ("a", "b") where a is not null;"#
        );
        assert_eq!(down_sql(&changes), r#"drop index "uq_ab";"#);
    }

    #[test]
    fn changing_an_index_drops_then_recreates_it() {
        let columns = vec![col("id", "bigint"), col("a", "bigint"), col("b", "bigint")];
        let old = table_with_indexes("event", columns.clone(), vec![index("i", &["a"])]);
        let new = table_with_indexes("event", columns, vec![index("i", &["a", "b"])]);
        let up = up_sql(&diff(&old, &new, &mut NeverRename));
        assert!(up.contains(r#"drop index "i";"#), "drop first: {up}");
        assert!(
            up.contains(r#"create index "i" on "event" ("a", "b");"#),
            "recreate: {up}"
        );
        assert!(
            up.find("drop index").unwrap() < up.find("create index").unwrap(),
            "drop must precede recreate"
        );
    }

    #[test]
    fn widening_warnings_flags_rls_disable_and_unforce_only() {
        let changes = vec![
            Change::SetRls {
                table: "posts".to_owned(),
                enabled: false,
            },
            Change::SetForceRls {
                table: "posts".to_owned(),
                forced: false,
            },
            // These do NOT widen access -> no warning.
            Change::SetRls {
                table: "posts".to_owned(),
                enabled: true,
            },
            Change::SetForceRls {
                table: "posts".to_owned(),
                forced: true,
            },
        ];
        let warnings = widening_warnings(&changes);
        assert_eq!(warnings.len(), 2, "got: {warnings:?}");
        assert!(warnings[0].contains("disables row-level security"));
        assert!(warnings[1].contains("un-forces"));
    }
}
