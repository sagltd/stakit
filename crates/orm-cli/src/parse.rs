//! Parse `#[derive(Table)]` structs from a Rust source tree into a [`Schema`]
//! using `syn` only — no compilation, no database.
//!
//! Module-aware: starting from an entry file it follows `mod name;` declarations to
//! their files (`<dir>/name.rs` or `<dir>/name/mod.rs`, honoring `#[path = "..."]`)
//! and recurses inline `mod name { ... }` blocks, so a schema split across many files
//! (e.g. `schema.rs` with `mod user_schema;` in `schema/user_schema.rs`) is fully read.

use crate::model::{Column, ForeignKey, Schema, Table};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use syn::{
    Attribute, Data, DeriveInput, Fields, GenericArgument, Item, ItemMod, PathArguments, Type,
};

/// Parse all tables reachable from the entry schema file, following `mod`
/// declarations across files and inline modules.
///
/// # Errors
/// Returns a message if a file can't be read/parsed, a referenced module file is
/// missing, or a column type is unknown.
pub fn parse_schema_path(entry: &Path) -> Result<Schema, String> {
    let mut inputs = Vec::new();
    collect_file(entry, &mut inputs)?;
    build_schema(&inputs)
}

/// Parse all tables from a single source string (inline modules are followed; a bare
/// `mod name;` errors, since there is no file tree to resolve against).
///
/// # Errors
/// Returns a message if the source can't be parsed or a column type is unknown.
#[cfg(test)]
pub fn parse_schema(source: &str) -> Result<Schema, String> {
    let file = syn::parse_file(source).map_err(|error| error.to_string())?;
    let mut inputs = Vec::new();
    // No file context: child-file mods can't resolve; inline mods still recurse.
    collect_items(&file.items, Path::new("."), Path::new("."), &mut inputs)?;
    build_schema(&inputs)
}

/// Build a [`Schema`] from collected `#[derive(Table)]` inputs (two passes so FK
/// paths resolve against every table, regardless of which file declared them).
fn build_schema(inputs: &[DeriveInput]) -> Result<Schema, String> {
    let mut table_of_ident: HashMap<String, String> = HashMap::new();
    for input in inputs {
        table_of_ident.insert(input.ident.to_string(), table_name(input)?);
    }
    let mut tables = Vec::new();
    for input in inputs {
        tables.push(parse_table(input, &table_of_ident)?);
    }
    tables.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Schema { tables })
}

/// Read one file, then walk its items (recursing modules) collecting Table inputs.
fn collect_file(path: &Path, out: &mut Vec<DeriveInput>) -> Result<(), String> {
    let source = std::fs::read_to_string(path)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    let file =
        syn::parse_file(&source).map_err(|error| format!("parse {}: {error}", path.display()))?;
    let decl_dir = path.parent().unwrap_or_else(|| Path::new("."));
    collect_items(&file.items, decl_dir, &child_base_dir(path), out)?;
    Ok(())
}

/// Walk items: keep `#[derive(Table)]` structs; recurse inline `mod`s; follow
/// `mod name;` to its file. `decl_dir` is the directory of the file holding these
/// items (for `#[path]`); `base` is where child module files live.
fn collect_items(
    items: &[Item],
    decl_dir: &Path,
    base: &Path,
    out: &mut Vec<DeriveInput>,
) -> Result<(), String> {
    for item in items {
        match item {
            Item::Struct(item_struct) => {
                let input: DeriveInput = item_struct.clone().into();
                if derives_table(&input.attrs) {
                    out.push(input);
                }
            }
            Item::Mod(item_mod) => collect_mod(item_mod, decl_dir, base, out)?,
            _ => {}
        }
    }
    Ok(())
}

/// Handle a `mod` item: inline (`mod m { .. }`) recurses in place; a bare `mod m;`
/// resolves to a file and is read.
fn collect_mod(
    item_mod: &ItemMod,
    decl_dir: &Path,
    base: &Path,
    out: &mut Vec<DeriveInput>,
) -> Result<(), String> {
    let name = item_mod.ident.to_string();
    if let Some((_, items)) = &item_mod.content {
        // Inline module: a file-mod inside it resolves under `<base>/<name>/`.
        collect_items(items, &decl_dir.join(&name), &base.join(&name), out)
    } else {
        let file = resolve_mod_file(decl_dir, base, &name, mod_path_attr(&item_mod.attrs))?;
        collect_file(&file, out)
    }
}

/// The directory where a file's child modules live: same dir for `mod.rs`/crate
/// roots, otherwise a subdir named after the file stem (Rust 2018 module rules).
fn child_base_dir(path: &Path) -> PathBuf {
    let parent = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if matches!(stem, "mod" | "lib" | "main") {
        parent
    } else {
        parent.join(stem)
    }
}

/// Read the `#[path = "..."]` attribute of a module, if present.
fn mod_path_attr(attrs: &[Attribute]) -> Option<String> {
    for attr in attrs {
        if !attr.path().is_ident("path") {
            continue;
        }
        if let syn::Meta::NameValue(nv) = &attr.meta {
            if let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) = &nv.value
            {
                return Some(s.value());
            }
        }
    }
    None
}

/// Resolve a `mod name;` to a file: `#[path]` (relative to the declaring file's dir)
/// wins; otherwise `<base>/name.rs` then `<base>/name/mod.rs`.
fn resolve_mod_file(
    decl_dir: &Path,
    base: &Path,
    name: &str,
    path_attr: Option<String>,
) -> Result<PathBuf, String> {
    if let Some(rel) = path_attr {
        let candidate = decl_dir.join(rel);
        return if candidate.is_file() {
            Ok(candidate)
        } else {
            Err(format!(
                "module `{name}`: #[path] file not found: {}",
                candidate.display()
            ))
        };
    }
    let flat = base.join(format!("{name}.rs"));
    if flat.is_file() {
        return Ok(flat);
    }
    let nested = base.join(name).join("mod.rs");
    if nested.is_file() {
        return Ok(nested);
    }
    Err(format!(
        "module `{name}`: no file found (looked for {} and {})",
        flat.display(),
        nested.display()
    ))
}

fn derives_table(attrs: &[Attribute]) -> bool {
    let mut found = false;
    for attr in attrs {
        if !attr.path().is_ident("derive") {
            continue;
        }
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("Table") {
                found = true;
            }
            Ok(())
        });
    }
    found
}

fn table_name(input: &DeriveInput) -> Result<String, String> {
    for attr in &input.attrs {
        if !attr.path().is_ident("table") {
            continue;
        }
        let mut name = None;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
                name = Some(meta.value()?.parse::<syn::LitStr>()?.value());
            }
            Ok(())
        })
        .map_err(|error| error.to_string())?;
        if let Some(name) = name {
            return Ok(name);
        }
    }
    Err(format!("{}: missing #[table(name = \"...\")]", input.ident))
}

fn parse_table(
    input: &DeriveInput,
    table_of_ident: &HashMap<String, String>,
) -> Result<Table, String> {
    let Data::Struct(data) = &input.data else {
        return Err(format!("{}: not a struct", input.ident));
    };
    let Fields::Named(fields) = &data.fields else {
        return Err(format!("{}: needs named fields", input.ident));
    };
    let mut columns = Vec::new();
    for field in &fields.named {
        if unwrap_generic(&field.ty, "Rel").is_some() {
            continue; // relation marker, not a column
        }
        columns.push(parse_column(field, table_of_ident)?);
    }
    Ok(Table {
        name: table_name(input)?,
        columns,
    })
}

fn parse_column(
    field: &syn::Field,
    table_of_ident: &HashMap<String, String>,
) -> Result<Column, String> {
    let ident = field.ident.as_ref().expect("named field");
    let mut name = ident.to_string();
    let mut pk = false;
    let mut unique = false;
    let mut index = false;
    let mut nullable = unwrap_generic(&field.ty, "Option").is_some();
    let mut default = None;
    let mut explicit_type = None;
    let mut references: Option<ForeignKey> = None;
    let mut on_delete: Option<String> = None;

    for attr in &field.attrs {
        if !attr.path().is_ident("column") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("pk") {
                pk = true;
            } else if meta.path.is_ident("unique") {
                unique = true;
            } else if meta.path.is_ident("index") {
                index = true;
            } else if meta.path.is_ident("nullable") {
                nullable = true;
            } else if meta.path.is_ident("name") {
                name = meta.value()?.parse::<syn::LitStr>()?.value();
            } else if meta.path.is_ident("default") {
                default = Some(meta.value()?.parse::<syn::LitStr>()?.value());
            } else if meta.path.is_ident("sql_type") {
                explicit_type = Some(meta.value()?.parse::<syn::LitStr>()?.value());
            } else if meta.path.is_ident("references") {
                references = Some(parse_reference(&meta, table_of_ident)?);
            } else if meta.path.is_ident("on_delete") {
                on_delete = Some(meta.value()?.parse::<syn::LitStr>()?.value());
            }
            Ok(())
        })
        .map_err(|error| error.to_string())?;
    }

    if let (Some(fk), Some(action)) = (references.as_mut(), on_delete) {
        fk.on_delete = action;
    }

    let base = unwrap_generic(&field.ty, "Option").unwrap_or(&field.ty);
    let sql_type = match explicit_type {
        Some(explicit) => explicit,
        None => sql_type(base).ok_or_else(|| {
            format!("{name}: unknown SQL type; add #[column(sql_type = \"...\")]")
        })?,
    };

    Ok(Column {
        name,
        sql_type,
        nullable,
        pk,
        unique,
        index,
        default,
        references,
    })
}

fn parse_reference(
    meta: &syn::meta::ParseNestedMeta<'_>,
    table_of_ident: &HashMap<String, String>,
) -> syn::Result<ForeignKey> {
    let path: syn::Path = meta.value()?.parse()?;
    let segments: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
    let column = segments.last().cloned().unwrap_or_default();
    let root = segments.iter().rev().nth(1).cloned().unwrap_or_default();
    let table = table_of_ident.get(&root).cloned().unwrap_or(root);
    Ok(ForeignKey {
        table,
        column,
        on_delete: "no action".to_owned(),
    })
}

fn unwrap_generic<'a>(ty: &'a Type, wrapper: &str) -> Option<&'a Type> {
    let Type::Path(type_path) = ty else {
        return None;
    };
    let segment = type_path.path.segments.last()?;
    if segment.ident != wrapper {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &segment.arguments else {
        return None;
    };
    args.args.iter().find_map(|arg| match arg {
        GenericArgument::Type(inner) => Some(inner),
        _ => None,
    })
}

fn last_ident(ty: &Type) -> Option<String> {
    let Type::Path(type_path) = ty else {
        return None;
    };
    Some(type_path.path.segments.last()?.ident.to_string())
}

fn sql_type(ty: &Type) -> Option<String> {
    if let Some(inner) = unwrap_generic(ty, "Vec") {
        if last_ident(inner).as_deref() == Some("u8") {
            return Some("bytea".to_owned());
        }
        return None;
    }
    let mapped = match last_ident(ty)?.as_str() {
        "i16" => "smallint",
        "i32" => "int",
        "i64" => "bigint",
        "f32" => "real",
        "f64" => "double precision",
        "bool" => "boolean",
        "String" | "str" => "text",
        "Uuid" => "uuid",
        "DateTime" => "timestamptz",
        "NaiveDateTime" => "timestamp",
        "NaiveDate" => "date",
        "NaiveTime" => "time",
        "Value" => "jsonb",
        _ => return None,
    };
    Some(mapped.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{parse_schema, parse_schema_path};

    #[test]
    fn follows_file_and_inline_modules_across_the_tree() {
        // Build a temp module tree:
        //   schema.rs            -> mod user_schema;  + inline mod billing { Invoice }
        //   schema/user_schema.rs-> User, and `mod posts;`
        //   schema/user_schema/posts.rs -> Post (FK -> User in a sibling file)
        let root = std::env::temp_dir().join(format!("stakit_cli_parse_{}", std::process::id()));
        let sub = root.join("schema").join("user_schema");
        std::fs::create_dir_all(&sub).unwrap();

        std::fs::write(
            root.join("schema.rs"),
            r#"
                mod user_schema;
                mod billing {
                    #[derive(Table)]
                    #[table(name = "invoices")]
                    struct Invoice { #[column(pk)] id: i64 }
                }
            "#,
        )
        .unwrap();
        std::fs::write(
            root.join("schema").join("user_schema.rs"),
            r#"
                mod posts;
                #[derive(Table)]
                #[table(name = "users")]
                struct User { #[column(pk)] id: i64, #[column(unique)] email: String }
            "#,
        )
        .unwrap();
        std::fs::write(
            sub.join("posts.rs"),
            r#"
                #[derive(Table)]
                #[table(name = "posts")]
                struct Post {
                    #[column(pk)] id: i64,
                    #[column(references = User::id, on_delete = "cascade")] author_id: i64,
                }
            "#,
        )
        .unwrap();

        let schema = parse_schema_path(&root.join("schema.rs")).unwrap();
        std::fs::remove_dir_all(&root).ok();

        // All three tables, from three files + an inline module, are present.
        let names: Vec<&str> = schema.tables.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["invoices", "posts", "users"]);
        // FK across files resolves (Post.author_id -> users.id).
        let fk = schema
            .table("posts")
            .unwrap()
            .column("author_id")
            .unwrap()
            .references
            .as_ref()
            .unwrap();
        assert_eq!(fk.table, "users");
        assert_eq!(fk.column, "id");
        assert_eq!(fk.on_delete, "cascade");
    }

    #[test]
    fn missing_module_file_is_reported() {
        let root = std::env::temp_dir().join(format!("stakit_cli_missing_{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("schema.rs"), "mod nope;\n").unwrap();
        let result = parse_schema_path(&root.join("schema.rs"));
        std::fs::remove_dir_all(&root).ok();
        let error = result.unwrap_err();
        assert!(error.contains("module `nope`"), "got: {error}");
    }

    #[test]
    fn parses_table_with_fk() {
        let source = r#"
            #[derive(Table)]
            #[table(name = "users")]
            struct User { #[column(pk)] id: Uuid, #[column(unique)] email: String }

            #[derive(Table)]
            #[table(name = "posts")]
            struct Post {
                #[column(pk)] id: Uuid,
                #[column(references = User::id, on_delete = "cascade")] author_id: Uuid,
                body: Option<String>,
            }
        "#;
        let schema = parse_schema(source).unwrap();
        assert_eq!(schema.tables.len(), 2);
        let posts = schema.table("posts").unwrap();
        let author = posts.column("author_id").unwrap();
        let fk = author.references.as_ref().unwrap();
        assert_eq!(fk.table, "users");
        assert_eq!(fk.column, "id");
        assert_eq!(fk.on_delete, "cascade");
        let body = posts.column("body").unwrap();
        assert!(body.nullable);
        assert_eq!(body.sql_type, "text");
    }
}
