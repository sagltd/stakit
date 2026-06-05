//! Parse `#[derive(Table)]` structs from a Rust source tree into a [`Schema`]
//! using `syn` only — no compilation, no database.
//!
//! Module-aware: starting from an entry file it follows `mod name;` declarations to
//! their files (`<dir>/name.rs` or `<dir>/name/mod.rs`, honoring `#[path = "..."]`)
//! and recurses inline `mod name { ... }` blocks, so a schema split across many files
//! (e.g. `schema.rs` with `mod user_schema;` in `schema/user_schema.rs`) is fully read.

use crate::model::{
    Column, ForeignKey, Grant, Policy, PolicyCommand, Privilege, Role, Schema, Table,
};
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

/// Build a [`Schema`] from collected `#[derive(Table)]` / `#[derive(Role)]` inputs
/// (two passes over tables so FK paths resolve against every table, regardless of
/// which file declared them).
fn build_schema(inputs: &[DeriveInput]) -> Result<Schema, String> {
    let table_inputs: Vec<&DeriveInput> = inputs
        .iter()
        .filter(|input| derives_table(&input.attrs))
        .collect();

    let mut table_of_ident: HashMap<String, String> = HashMap::new();
    for input in &table_inputs {
        table_of_ident.insert(input.ident.to_string(), table_meta(input)?.name);
    }
    let mut tables = Vec::new();
    for input in &table_inputs {
        tables.push(parse_table(input, &table_of_ident)?);
    }
    tables.sort_by(|a, b| a.name.cmp(&b.name));

    let mut roles = Vec::new();
    for input in inputs {
        if derives_role(&input.attrs) {
            roles.push(parse_role(input)?);
        }
    }
    roles.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(Schema { tables, roles })
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
                if derives_table(&input.attrs) || derives_role(&input.attrs) {
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

/// Whether `attrs` contains `#[derive(<name>)]` for the given derive macro.
fn derives(attrs: &[Attribute], name: &str) -> bool {
    let mut found = false;
    for attr in attrs {
        if !attr.path().is_ident("derive") {
            continue;
        }
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident(name) {
                found = true;
            }
            Ok(())
        });
    }
    found
}

fn derives_table(attrs: &[Attribute]) -> bool {
    derives(attrs, "Table")
}

fn derives_role(attrs: &[Attribute]) -> bool {
    derives(attrs, "Role")
}

/// Parsed `#[table(...)]` metadata, including nested RLS config.
struct TableMeta {
    name: String,
    rls: bool,
    force_rls: bool,
    policies: Vec<Policy>,
    grants: Vec<Grant>,
}

/// Parse `#[table(name = "...", rls, force_rls, grant(...), policy(...))]`. RLS config
/// is nested inside `#[table(...)]` (not separate attributes) to match the derive and
/// avoid clippy's cross-attribute `duplicated_attributes`. Unknown keys are rejected so
/// a typo fails loudly rather than silently disabling RLS.
fn table_meta(input: &DeriveInput) -> Result<TableMeta, String> {
    let mut name = None;
    let mut rls = false;
    let mut force_rls = false;
    let mut policies = Vec::new();
    let mut grants: Vec<Grant> = Vec::new();
    for attr in &input.attrs {
        if !attr.path().is_ident("table") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
                name = Some(meta.value()?.parse::<syn::LitStr>()?.value());
            } else if meta.path.is_ident("rls") {
                rls = true;
            } else if meta.path.is_ident("force_rls") {
                force_rls = true;
            } else if meta.path.is_ident("grant") {
                meta.parse_nested_meta(|entry| {
                    let grant =
                        parse_grant_entry(&entry).map_err(|message| entry.error(message))?;
                    merge_grants(&mut grants, vec![grant]);
                    Ok(())
                })?;
            } else if meta.path.is_ident("policy") {
                meta.parse_nested_meta(|entry| {
                    let policy =
                        parse_policy_entry(&entry).map_err(|message| entry.error(message))?;
                    policies.push(policy);
                    Ok(())
                })?;
            } else {
                return Err(meta.error("unknown #[table(...)] attribute"));
            }
            Ok(())
        })
        .map_err(|error| error.to_string())?;
    }
    let name = name.ok_or_else(|| format!("{}: missing #[table(name = \"...\")]", input.ident))?;
    validate_identifier(&name)?;
    if force_rls && !rls {
        return Err(format!("{name}: force_rls requires rls on the same table"));
    }
    // A policy is dead without RLS enabled — reject the silent footgun.
    if !policies.is_empty() && !rls {
        return Err(format!(
            "{name}: policy(...) requires RLS — add `rls` to #[table(name = \"...\", rls, …)]"
        ));
    }
    policies.sort_by(|a, b| a.name.cmp(&b.name));
    grants.sort_by(|a, b| a.role.cmp(&b.role));
    Ok(TableMeta {
        name,
        rls,
        force_rls,
        policies,
        grants,
    })
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

    let meta = table_meta(input)?;
    Ok(Table {
        name: meta.name,
        columns,
        rls: meta.rls,
        force_rls: meta.force_rls,
        policies: meta.policies,
        grants: meta.grants,
    })
}

/// Merge `incoming` grants into `grants`, unioning privileges for a repeated role so
/// two `#[grant(to = "r", …)]` on the same role combine rather than duplicate.
fn merge_grants(grants: &mut Vec<Grant>, incoming: Vec<Grant>) {
    for new in incoming {
        if let Some(existing) = grants.iter_mut().find(|grant| grant.role == new.role) {
            for privilege in new.privileges {
                if !existing.privileges.contains(&privilege) {
                    existing.privileges.push(privilege);
                }
            }
        } else {
            grants.push(new);
        }
    }
}

/// Parse one `#[role(name = "...", login, createdb, createrole, bypassrls)]`.
fn parse_role(input: &DeriveInput) -> Result<Role, String> {
    let mut name = None;
    let mut login = false;
    let mut createdb = false;
    let mut createrole = false;
    let mut bypassrls = false;
    for attr in &input.attrs {
        if !attr.path().is_ident("role") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
                name = Some(meta.value()?.parse::<syn::LitStr>()?.value());
            } else if meta.path.is_ident("login") {
                login = true;
            } else if meta.path.is_ident("createdb") {
                createdb = true;
            } else if meta.path.is_ident("createrole") {
                createrole = true;
            } else if meta.path.is_ident("bypassrls") {
                bypassrls = true;
            } else {
                return Err(meta.error("unknown #[role(...)] attribute"));
            }
            Ok(())
        })
        .map_err(|error| error.to_string())?;
    }
    let name = name.ok_or_else(|| format!("{}: missing #[role(name = \"...\")]", input.ident))?;
    validate_identifier(&name)?;
    Ok(Role {
        name,
        login,
        createdb,
        createrole,
        bypassrls,
    })
}

/// Parse one `policy(<name>(<command>, to = "...", using = "...", check = "..."))`
/// entry. The policy name is the entry's head identifier, so two policies are distinct
/// nested lists (clippy's `duplicated_attributes` never sees a repeated `to = "role"`).
fn parse_policy_entry(entry: &syn::meta::ParseNestedMeta<'_>) -> Result<Policy, String> {
    let name = entry
        .path
        .require_ident()
        .map_err(|error| error.to_string())?
        .to_string();
    validate_identifier(&name)?;
    let mut command = None;
    let mut roles = Vec::new();
    let mut to_present = false;
    let mut using = None;
    let mut check = None;
    entry
        .parse_nested_meta(|inner| {
            if inner.path.is_ident("to") {
                to_present = true;
                roles = split_roles(&inner.value()?.parse::<syn::LitStr>()?.value());
            } else if inner.path.is_ident("using") {
                using = Some(inner.value()?.parse::<syn::LitStr>()?.value());
            } else if inner.path.is_ident("check") {
                check = Some(inner.value()?.parse::<syn::LitStr>()?.value());
            } else if let Some(parsed) = policy_command(&inner.path) {
                if command.is_some() {
                    return Err(inner.error(
                        "policy has more than one command (all/select/insert/update/delete)",
                    ));
                }
                command = Some(parsed);
            } else {
                return Err(inner.error(
                    "unknown policy entry attribute (expected a command, to, using, check)",
                ));
            }
            Ok(())
        })
        .map_err(|error| error.to_string())?;

    // `to = ""` (or all-empty) would silently broaden the policy to PUBLIC — reject it.
    // PUBLIC is reachable only by omitting `to` entirely.
    if to_present && roles.is_empty() {
        return Err(format!(
            "policy `{name}`: `to` was specified but lists no roles (omit `to` for PUBLIC)"
        ));
    }
    for role in &roles {
        validate_identifier(role)?;
    }
    let command = command.unwrap_or(PolicyCommand::All);
    validate_policy_clauses(&name, command, using.as_deref(), check.as_deref())?;
    Ok(Policy {
        name,
        command,
        roles,
        using,
        check,
    })
}

/// Parse one `grant(<role>(<privilege>...))` entry. The role is the entry's head
/// identifier (lint-safe, like policies), with the privileges as bare flags.
fn parse_grant_entry(entry: &syn::meta::ParseNestedMeta<'_>) -> Result<Grant, String> {
    let role = entry
        .path
        .require_ident()
        .map_err(|error| error.to_string())?
        .to_string();
    validate_identifier(&role)?;
    let mut privileges: Vec<Privilege> = Vec::new();
    entry
        .parse_nested_meta(|inner| {
            let Some(privilege) = grant_privilege(&inner.path) else {
                return Err(inner.error("expected a privilege (select/insert/update/delete/all)"));
            };
            if !privileges.contains(&privilege) {
                privileges.push(privilege);
            }
            Ok(())
        })
        .map_err(|error| error.to_string())?;

    if privileges.is_empty() {
        return Err(format!(
            "grant role `{role}` needs at least one privilege (select/insert/update/delete/all)"
        ));
    }
    Ok(Grant { role, privileges })
}

/// Map a bare attribute flag to a policy command, if it is one.
fn policy_command(path: &syn::Path) -> Option<PolicyCommand> {
    if path.is_ident("all") {
        Some(PolicyCommand::All)
    } else if path.is_ident("select") {
        Some(PolicyCommand::Select)
    } else if path.is_ident("insert") {
        Some(PolicyCommand::Insert)
    } else if path.is_ident("update") {
        Some(PolicyCommand::Update)
    } else if path.is_ident("delete") {
        Some(PolicyCommand::Delete)
    } else {
        None
    }
}

/// Map a bare attribute flag to a grant privilege, if it is one.
fn grant_privilege(path: &syn::Path) -> Option<Privilege> {
    if path.is_ident("all") {
        Some(Privilege::All)
    } else if path.is_ident("select") {
        Some(Privilege::Select)
    } else if path.is_ident("insert") {
        Some(Privilege::Insert)
    } else if path.is_ident("update") {
        Some(Privilege::Update)
    } else if path.is_ident("delete") {
        Some(Privilege::Delete)
    } else {
        None
    }
}

/// Split a comma-separated `to = "a, b"` into trimmed, non-empty role names.
fn split_roles(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|role| !role.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// Enforce the command/clause rules a policy must satisfy (mirrors Postgres):
/// SELECT/DELETE take `using` only, INSERT takes `check` only, ALL/UPDATE take at
/// least one. `using`/`check` are verbatim trusted SQL, so only emptiness is checked.
fn validate_policy_clauses(
    name: &str,
    command: PolicyCommand,
    using: Option<&str>,
    check: Option<&str>,
) -> Result<(), String> {
    let keyword = command.as_sql();
    match command {
        PolicyCommand::Select | PolicyCommand::Delete => {
            // Report the forbidden clause before the missing one (the likelier mistake).
            if check.is_some() {
                return Err(format!(
                    "policy `{name}`: a {keyword} policy cannot have check = \"...\" (no WITH CHECK on {keyword})"
                ));
            }
            if using.is_none() {
                return Err(format!(
                    "policy `{name}`: a {keyword} policy needs using = \"...\""
                ));
            }
        }
        PolicyCommand::Insert => {
            if using.is_some() {
                return Err(format!(
                    "policy `{name}`: an insert policy cannot have using = \"...\" (INSERT only checks new rows)"
                ));
            }
            if check.is_none() {
                return Err(format!(
                    "policy `{name}`: an insert policy needs check = \"...\""
                ));
            }
        }
        PolicyCommand::All | PolicyCommand::Update => {
            if using.is_none() && check.is_none() {
                return Err(format!(
                    "policy `{name}`: a {keyword} policy needs at least one of using = \"...\" / check = \"...\""
                ));
            }
        }
    }
    if using == Some("") || check == Some("") {
        return Err(format!(
            "policy `{name}`: using/check expression must not be empty"
        ));
    }
    Ok(())
}

/// Reject `on_delete` combinations the derive also rejects (its keyword is emitted
/// verbatim into the FK clause, so a free string would otherwise reach DDL raw). The
/// caller prepends the column name to the returned message.
fn validate_on_delete(action: &str, has_references: bool, nullable: bool) -> Result<(), String> {
    if !has_references {
        return Err("on_delete requires references".to_owned());
    }
    if !matches!(action, "cascade" | "restrict" | "set null" | "no action") {
        return Err("on_delete must be one of: cascade, restrict, set null, no action".to_owned());
    }
    if action == "set null" && !nullable {
        return Err("on_delete = \"set null\" requires a nullable (Option<_>) column".to_owned());
    }
    Ok(())
}

/// Reject identifiers Postgres cannot store safely (empty, embedded NUL, or longer
/// than NAMEDATALEN). Identifiers are quoted on emit, so other characters are fine.
fn validate_identifier(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("identifier is empty".to_owned());
    }
    if name.as_bytes().contains(&0) {
        return Err(format!("identifier `{name}` contains a NUL byte"));
    }
    if name.len() > 63 {
        return Err(format!(
            "identifier `{name}` exceeds Postgres NAMEDATALEN (63 bytes)"
        ));
    }
    Ok(())
}

// A flat attribute reader + the validations that mirror the derive; splitting it
// further would scatter one column's parsing across helpers without added clarity.
#[allow(clippy::too_many_lines)]
fn parse_column(
    field: &syn::Field,
    table_of_ident: &HashMap<String, String>,
) -> Result<Column, String> {
    let ident = field.ident.as_ref().expect("named field");
    let mut name = ident.to_string();
    let mut pk = false;
    let mut unique = false;
    let mut index = false;
    let mut index_method = None;
    let mut opclass = None;
    let mut generated = None;
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
                // `index = "hnsw"` is the access-method value form.
                if let Ok(value) = meta.value() {
                    index_method = Some(value.parse::<syn::LitStr>()?.value());
                }
            } else if meta.path.is_ident("index_method") {
                index_method = Some(meta.value()?.parse::<syn::LitStr>()?.value());
            } else if meta.path.is_ident("opclass") {
                opclass = Some(meta.value()?.parse::<syn::LitStr>()?.value());
            } else if meta.path.is_ident("nullable") {
                nullable = true;
            } else if meta.path.is_ident("name") {
                name = meta.value()?.parse::<syn::LitStr>()?.value();
            } else if meta.path.is_ident("default") {
                default = Some(meta.value()?.parse::<syn::LitStr>()?.value());
            } else if meta.path.is_ident("generated") {
                generated = Some(meta.value()?.parse::<syn::LitStr>()?.value());
            } else if meta.path.is_ident("sql_type") {
                explicit_type = Some(meta.value()?.parse::<syn::LitStr>()?.value());
            } else if meta.path.is_ident("references") {
                references = Some(parse_reference(&meta, table_of_ident)?);
            } else if meta.path.is_ident("on_delete") {
                on_delete = Some(meta.value()?.parse::<syn::LitStr>()?.value());
            } else {
                // Match the derive: an unknown key must fail, not be dropped.
                return Err(meta.error("unknown #[column(...)] attribute"));
            }
            Ok(())
        })
        .map_err(|error| error.to_string())?;
    }

    // The CLI is the only gate at `gen` time (gen does not compile the schema), so it
    // must reject the same identifiers/keywords the derive does — otherwise a name that
    // overflows NAMEDATALEN, or an `on_delete` string, reaches the (non-validating)
    // `quote()`/DDL emitter and can corrupt or break out of the generated SQL.
    validate_identifier(&name)?;
    if let Some(action) = &on_delete {
        validate_on_delete(action, references.is_some(), nullable)
            .map_err(|message| format!("{name}: {message}"))?;
    }
    if let (Some(fk), Some(action)) = (references.as_mut(), on_delete) {
        fk.on_delete = action;
    }

    // Mirror the derive's compile-time checks so the CLI rejects the same source the
    // macro would (otherwise it could emit DDL the compiler refuses to build against).
    if !index && (index_method.is_some() || opclass.is_some()) {
        return Err(format!(
            "{name}: index_method/opclass require #[column(index)] on the same column"
        ));
    }
    if let Some(method) = &index_method {
        if !is_sql_identifier(method, false) {
            return Err(format!(
                "{name}: index_method must be a bare SQL identifier"
            ));
        }
    }
    if let Some(opclass) = &opclass {
        if !is_sql_identifier(opclass, true) {
            return Err(format!(
                "{name}: opclass must be a (optionally schema-qualified) SQL identifier"
            ));
        }
    }
    if generated.is_some() && default.is_some() {
        return Err(format!(
            "{name}: a #[column(generated = ...)] column cannot also have a default"
        ));
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
        index_method,
        opclass,
        generated,
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
    // Both names land in the FK clause via the non-validating `quote()`; validate them
    // here so an over-NAMEDATALEN spelling is a hard error, not a silent truncation.
    validate_identifier(&table).map_err(|message| meta.error(message))?;
    validate_identifier(&column).map_err(|message| meta.error(message))?;
    Ok(ForeignKey {
        table,
        column,
        on_delete: "no action".to_owned(),
    })
}

/// Whether `value` is a SQL identifier: a non-empty run of ASCII letters, digits, and
/// underscores starting with a letter or underscore. When `allow_qualified`, a single
/// `schema.name` qualification is accepted (each part validated the same way).
fn is_sql_identifier(value: &str, allow_qualified: bool) -> bool {
    let parts: Vec<&str> = value.split('.').collect();
    if parts.len() > 1 && !allow_qualified {
        return false;
    }
    parts.iter().all(|part| {
        let mut chars = part.chars();
        matches!(chars.next(), Some(first) if first.is_ascii_alphabetic() || first == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
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
    fn parses_index_method_and_opclass() {
        let source = r#"
            #[derive(Table)]
            #[table(name = "docs")]
            struct Doc {
                #[column(pk)] id: i64,
                #[column(sql_type = "vector(3)", index, index_method = "hnsw", opclass = "vector_cosine_ops")]
                embedding: Vector,
            }
        "#;
        let schema = parse_schema(source).unwrap();
        let embedding = schema.table("docs").unwrap().column("embedding").unwrap();
        assert!(embedding.index);
        assert_eq!(embedding.index_method.as_deref(), Some("hnsw"));
        assert_eq!(embedding.opclass.as_deref(), Some("vector_cosine_ops"));
    }

    #[test]
    fn index_value_form_sets_method() {
        let source = r#"
            #[derive(Table)]
            #[table(name = "places")]
            struct Place {
                #[column(pk)] id: i64,
                #[column(sql_type = "geometry", index = "gist")] location: GeoPoint,
            }
        "#;
        let schema = parse_schema(source).unwrap();
        let location = schema.table("places").unwrap().column("location").unwrap();
        assert_eq!(location.index_method.as_deref(), Some("gist"));
    }

    #[test]
    fn unknown_column_attribute_is_rejected() {
        let source = r#"
            #[derive(Table)]
            #[table(name = "docs")]
            struct Doc {
                #[column(pk)] id: i64,
                #[column(index_methd = "hnsw")] body: String,
            }
        "#;
        let error = parse_schema(source).unwrap_err();
        assert!(error.contains("unknown"), "got: {error}");
    }

    #[test]
    fn opclass_without_index_is_rejected() {
        let source = r#"
            #[derive(Table)]
            #[table(name = "docs")]
            struct Doc {
                #[column(pk)] id: i64,
                #[column(opclass = "vector_cosine_ops")] embedding: Vector,
            }
        "#;
        let error = parse_schema(source).unwrap_err();
        assert!(error.contains("require #[column(index)]"), "got: {error}");
    }

    #[test]
    fn generated_with_default_is_rejected() {
        let source = r#"
            #[derive(Table)]
            #[table(name = "docs")]
            struct Doc {
                #[column(pk)] id: i64,
                #[column(sql_type = "tsvector", generated = "to_tsvector('english', body)", default = "''")]
                body_tsv: String,
            }
        "#;
        let error = parse_schema(source).unwrap_err();
        assert!(error.contains("cannot also have a default"), "got: {error}");
    }

    #[test]
    fn index_method_with_sql_breakout_is_rejected() {
        let source = r#"
            #[derive(Table)]
            #[table(name = "docs")]
            struct Doc {
                #[column(pk)] id: i64,
                #[column(sql_type = "vector(3)", index, index_method = "gin); drop table users; --")]
                embedding: Vector,
            }
        "#;
        let error = parse_schema(source).unwrap_err();
        assert!(error.contains("bare SQL identifier"), "got: {error}");
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

    /// Regression: `build_schema` used `Schema { tables }` (missing `roles`), causing a
    /// compile error after `Schema` gained a `roles` field. `parse_schema` must still
    /// return a `Schema` with `roles` defaulting to an empty vec.
    #[test]
    fn build_schema_populates_roles_as_empty_vec() {
        let source = r#"
            #[derive(Table)]
            #[table(name = "items")]
            struct Item { #[column(pk)] id: String }
        "#;
        let schema = parse_schema(source).unwrap();
        assert!(schema.roles.is_empty(), "roles should default to empty vec");
        assert_eq!(schema.tables.len(), 1);
    }

    // ---- Row-level security parsing ----

    use crate::model::{PolicyCommand, Privilege};

    #[test]
    fn parses_role_with_attributes() {
        let source = r#"
            #[derive(Role)]
            #[role(name = "app_user", login, bypassrls)]
            struct AppUser;
        "#;
        let schema = parse_schema(source).unwrap();
        let role = schema.role("app_user").expect("role parsed");
        assert!(role.login);
        assert!(role.bypassrls);
        assert!(!role.createdb);
        assert!(!role.createrole);
    }

    #[test]
    fn parses_table_rls_grant_and_policy() {
        let source = r#"
            #[derive(Table)]
            #[table(name = "posts", rls, force_rls,
                grant(app_user(select, insert)),
                policy(owner(select, to = "app_user", using = "author_id = current_user_id()")))]
            struct Post { #[column(pk)] id: String, author_id: String }
        "#;
        let schema = parse_schema(source).unwrap();
        let posts = schema.table("posts").unwrap();
        assert!(posts.rls);
        assert!(posts.force_rls);
        let grant = posts.grant("app_user").expect("grant");
        assert_eq!(grant.privileges, vec![Privilege::Select, Privilege::Insert]);
        let policy = posts.policy("owner").expect("policy");
        assert_eq!(policy.command, PolicyCommand::Select);
        assert_eq!(policy.roles, vec!["app_user".to_owned()]);
        assert_eq!(
            policy.using.as_deref(),
            Some("author_id = current_user_id()")
        );
        assert!(policy.check.is_none());
    }

    #[test]
    fn grant_to_multiple_roles_expands_per_role() {
        let source = r#"
            #[derive(Table)]
            #[table(name = "posts", grant(reader(select), writer(select)))]
            struct Post { #[column(pk)] id: String }
        "#;
        let posts_schema = parse_schema(source).unwrap();
        let posts = posts_schema.table("posts").unwrap();
        assert_eq!(posts.grants.len(), 2);
        assert!(posts.grant("reader").is_some());
        assert!(posts.grant("writer").is_some());
    }

    #[test]
    fn repeated_grant_for_same_role_merges_privileges() {
        let source = r#"
            #[derive(Table)]
            #[table(name = "posts", grant(app_user(select), app_user(insert)))]
            struct Post { #[column(pk)] id: String }
        "#;
        let posts_schema = parse_schema(source).unwrap();
        let grant = posts_schema
            .table("posts")
            .unwrap()
            .grant("app_user")
            .unwrap();
        assert_eq!(grant.privileges, vec![Privilege::Select, Privilege::Insert]);
    }

    #[test]
    fn policy_without_rls_is_rejected() {
        let source = r#"
            #[derive(Table)]
            #[table(name = "posts", policy(owner(select, using = "true")))]
            struct Post { #[column(pk)] id: String }
        "#;
        let error = parse_schema(source).unwrap_err();
        assert!(error.contains("requires RLS"), "got: {error}");
    }

    #[test]
    fn force_rls_without_rls_is_rejected() {
        let source = r#"
            #[derive(Table)]
            #[table(name = "posts", force_rls)]
            struct Post { #[column(pk)] id: String }
        "#;
        let error = parse_schema(source).unwrap_err();
        assert!(error.contains("force_rls requires rls"), "got: {error}");
    }

    #[test]
    fn insert_policy_with_using_is_rejected() {
        let source = r#"
            #[derive(Table)]
            #[table(name = "posts", rls, policy(ins(insert, using = "true")))]
            struct Post { #[column(pk)] id: String }
        "#;
        let error = parse_schema(source).unwrap_err();
        assert!(
            error.contains("insert policy cannot have using"),
            "got: {error}"
        );
    }

    #[test]
    fn select_policy_without_using_is_rejected() {
        let source = r#"
            #[derive(Table)]
            #[table(name = "posts", rls, policy(sel(select)))]
            struct Post { #[column(pk)] id: String }
        "#;
        let error = parse_schema(source).unwrap_err();
        assert!(error.contains("needs using"), "got: {error}");
    }

    #[test]
    fn unknown_table_attribute_is_rejected() {
        let source = r#"
            #[derive(Table)]
            #[table(name = "posts", rsl)]
            struct Post { #[column(pk)] id: String }
        "#;
        let error = parse_schema(source).unwrap_err();
        assert!(error.contains("unknown #[table"), "got: {error}");
    }

    #[test]
    fn grant_without_privileges_is_rejected() {
        // A role with no privileges is rejected (syn requires the `role(<privs>)` group).
        let source = r#"
            #[derive(Table)]
            #[table(name = "posts", grant(app_user()))]
            struct Post { #[column(pk)] id: String }
        "#;
        assert!(parse_schema(source).is_err());
    }

    #[test]
    fn over_long_role_name_is_rejected() {
        let long = "r".repeat(64);
        let source = format!(
            r#"
            #[derive(Role)]
            #[role(name = "{long}")]
            struct R;
            "#
        );
        let error = parse_schema(&source).unwrap_err();
        assert!(error.contains("NAMEDATALEN"), "got: {error}");
    }

    #[test]
    fn policy_with_empty_to_is_rejected() {
        // `to = ""` must NOT silently become a PUBLIC policy.
        let source = r#"
            #[derive(Table)]
            #[table(name = "secrets", rls, policy(p(select, to = "", using = "true")))]
            struct Secret { #[column(pk)] id: String }
        "#;
        let error = parse_schema(source).unwrap_err();
        assert!(error.contains("lists no roles"), "got: {error}");
    }

    // The CLI must reject the same `on_delete` the derive does — `gen` doesn't compile,
    // so the CLI is the only gate (and a free string would otherwise reach DDL raw).
    #[test]
    fn on_delete_injection_keyword_is_rejected() {
        let source = r#"
            #[derive(Table)]
            #[table(name = "parent")]
            struct Parent { #[column(pk)] id: i64 }
            #[derive(Table)]
            #[table(name = "child")]
            struct Child {
                #[column(pk)] id: i64,
                #[column(references = Parent::id, on_delete = "cascade); drop table parent; --")]
                p: i64,
            }
        "#;
        let error = parse_schema(source).unwrap_err();
        assert!(error.contains("on_delete must be one of"), "got: {error}");
    }

    #[test]
    fn on_delete_without_references_is_rejected() {
        let source = r#"
            #[derive(Table)]
            #[table(name = "t")]
            struct T { #[column(pk)] id: i64, #[column(on_delete = "cascade")] x: i64 }
        "#;
        let error = parse_schema(source).unwrap_err();
        assert!(
            error.contains("on_delete requires references"),
            "got: {error}"
        );
    }

    #[test]
    fn set_null_on_not_null_column_is_rejected() {
        let source = r#"
            #[derive(Table)]
            #[table(name = "parent")]
            struct Parent { #[column(pk)] id: i64 }
            #[derive(Table)]
            #[table(name = "child")]
            struct Child {
                #[column(pk)] id: i64,
                #[column(references = Parent::id, on_delete = "set null")] p: i64,
            }
        "#;
        let error = parse_schema(source).unwrap_err();
        assert!(error.contains("requires a nullable"), "got: {error}");
    }

    #[test]
    fn over_long_column_name_is_rejected() {
        let long = "c".repeat(64);
        let source = format!(
            r#"
            #[derive(Table)]
            #[table(name = "t")]
            struct T {{ #[column(pk)] id: i64, #[column(name = "{long}")] x: i64 }}
            "#
        );
        let error = parse_schema(&source).unwrap_err();
        assert!(error.contains("NAMEDATALEN"), "got: {error}");
    }

    #[test]
    fn over_long_fk_reference_column_is_rejected() {
        // The referenced column SPELLING (not a real column on parent) is over-length —
        // exercises the FK-reference validation specifically.
        let long = "k".repeat(64);
        let source = format!(
            r#"
            #[derive(Table)]
            #[table(name = "parent")]
            struct Parent {{ #[column(pk)] id: i64 }}
            #[derive(Table)]
            #[table(name = "child")]
            struct Child {{
                #[column(pk)] id: i64,
                #[column(references = Parent::{long}, on_delete = "cascade")] p: i64,
            }}
            "#
        );
        let error = parse_schema(&source).unwrap_err();
        assert!(error.contains("NAMEDATALEN"), "got: {error}");
    }
}
