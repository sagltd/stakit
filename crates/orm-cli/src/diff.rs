//! Schema diffing and SQL generation. Pure functions (a [`Resolver`] supplies
//! interactive decisions), so the core is unit-testable without stdin.

use crate::model::{Column, Schema, Table};
use core::fmt::Write as _;

/// A single schema change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Create a new table.
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

/// Diff `old` → `new`, producing ordered changes.
pub fn diff(old: &Schema, new: &Schema, resolver: &mut dyn Resolver) -> Vec<Change> {
    let mut changes = Vec::new();
    for old_table in &old.tables {
        if new.table(&old_table.name).is_none() {
            changes.push(Change::DropTable(old_table.clone()));
        }
    }
    for new_table in &new.tables {
        match old.table(&new_table.name) {
            None => changes.push(Change::CreateTable(new_table.clone())),
            Some(old_table) => diff_table(old_table, new_table, resolver, &mut changes),
        }
    }
    changes
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
    if let Some(default) = &column.default {
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

/// `create index "idx_t_c" on "t" ("c");` (portable across Postgres/SQLite/MySQL).
fn create_index_sql(table: &str, column: &str) -> String {
    format!(
        "create index {} on {} ({});",
        quote(&index_name(table, column)),
        quote(table),
        quote(column)
    )
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

/// Render the forward (up) SQL for a change.
fn up(change: &Change) -> String {
    match change {
        Change::CreateTable(table) => {
            let mut sql = create_table_sql(table);
            for column in table.columns.iter().filter(|c| c.index) {
                sql.push('\n');
                sql.push_str(&create_index_sql(&table.name, &column.name));
            }
            sql
        }
        Change::DropTable(table) => format!("drop table {};", quote(&table.name)),
        Change::AddColumn { table, column } => {
            let mut sql = format!(
                "alter table {} add column {};",
                quote(table),
                column_ddl(column)
            );
            if column.index {
                sql.push('\n');
                sql.push_str(&create_index_sql(table, &column.name));
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
    }
}

/// Render the reverse (down) SQL for a change.
fn down(change: &Change) -> String {
    match change {
        Change::CreateTable(table) => format!("drop table {};", quote(&table.name)),
        Change::DropTable(table) => create_table_sql(table),
        Change::AddColumn { table, column } => {
            format!(
                "alter table {} drop column {};",
                quote(table),
                quote(&column.name)
            )
        }
        Change::DropColumn { table, column } => {
            format!(
                "alter table {} add column {};",
                quote(table),
                column_ddl(column)
            )
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
    use super::{Change, Resolver, diff, down_sql, up_sql};
    use crate::model::{Column, Schema, Table};

    fn col(name: &str, ty: &str) -> Column {
        Column {
            name: name.to_owned(),
            sql_type: ty.to_owned(),
            nullable: false,
            pk: name == "id",
            unique: false,
            index: false,
            default: None,
            references: None,
        }
    }

    fn table(name: &str, columns: Vec<Column>) -> Schema {
        Schema {
            tables: vec![Table {
                name: name.to_owned(),
                columns,
            }],
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
}
