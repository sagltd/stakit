//! Schema-attribute parsing shared by `stakit-orm-derive` and `stakit-orm-cli`.
//!
//! The derive validates `#[table(...)]` / `#[column(...)]` at compile time and the
//! CLI parses the same attributes into a migration model. Both read the same
//! syntax, so the readers live here once: Rust-type → SQL-type mapping, the
//! `#[table(...)]` head, and the row-level-security `policy(...)` / `grant(...)`
//! entries. Callers add their own identifier rules and error shapes on top.

use syn::meta::ParseNestedMeta;
use syn::{Attribute, GenericArgument, LitStr, Path, PathArguments, Type};

/// If `ty` is `Wrapper<Inner>` for the given `wrapper` ident, return `Inner`.
#[must_use]
pub fn unwrap_generic<'a>(ty: &'a Type, wrapper: &str) -> Option<&'a Type> {
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

/// Whether `ty` is a relation marker `Rel<...>` (not a column).
#[must_use]
pub fn is_relation(ty: &Type) -> bool {
    unwrap_generic(ty, "Rel").is_some()
}

/// The trailing identifier of a path type (e.g. `Uuid` from `uuid::Uuid`).
#[must_use]
pub fn last_ident(ty: &Type) -> Option<String> {
    let Type::Path(type_path) = ty else {
        return None;
    };
    Some(type_path.path.segments.last()?.ident.to_string())
}

/// Map a (non-`Option`) Rust type spelling to its canonical Postgres type.
///
/// Returns `None` for unknown spellings — the caller must then require an
/// explicit `#[column(sql_type = "...")]`.
#[must_use]
pub fn sql_type(ty: &Type) -> Option<&'static str> {
    // Vec<u8> -> bytea, special-cased; other arrays need an explicit sql_type.
    if let Some(inner) = unwrap_generic(ty, "Vec") {
        return (last_ident(inner).as_deref() == Some("u8")).then_some("bytea");
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
    Some(mapped)
}

/// Whether `value` is a SQL identifier.
///
/// That is a non-empty run of ASCII letters, digits, and underscores starting
/// with a letter or underscore. When `allow_qualified`, a single
/// `schema.name` qualification is accepted (each part validated the same way).
#[must_use]
pub fn is_sql_identifier(value: &str, allow_qualified: bool) -> bool {
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

/// Split a `"table.column"` foreign-key reference into its two identifiers.
///
/// Rejects anything that isn't exactly one `table` and one `column` (no extra dots).
///
/// # Errors
/// Returns the user-facing message when the reference is malformed.
pub fn split_table_column(reference: &str) -> Result<(String, String), &'static str> {
    let mut parts = reference.splitn(2, '.');
    let table = parts.next().unwrap_or("").trim().to_owned();
    let column = parts.next().map(|part| part.trim().to_owned());
    match column {
        Some(column) if !table.is_empty() && !column.is_empty() && !column.contains('.') => {
            Ok((table, column))
        }
        _ => Err("references string must be \"table.column\""),
    }
}

/// Split a comma-separated `to = "a, b"` into trimmed, non-empty role names.
#[must_use]
pub fn split_roles(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|role| !role.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// The `name` / `rls` / `force_rls` keys of a `#[table(...)]` attribute.
#[derive(Debug, Default)]
pub struct TableHead {
    /// `name = "..."`, if given.
    pub name: Option<String>,
    /// The `rls` flag.
    pub rls: bool,
    /// The `force_rls` flag.
    pub force_rls: bool,
}

/// Walk every `#[table(...)]` attribute.
///
/// Reads `name` / `rls` / `force_rls` into the returned [`TableHead`] and hands
/// every other key to `other` (which must
/// reject keys it does not know, so a typo fails loudly).
///
/// # Errors
/// Returns the first parse error, or whatever `other` returns.
pub fn parse_table_attrs(
    attrs: &[Attribute],
    mut other: impl FnMut(&ParseNestedMeta<'_>) -> syn::Result<()>,
) -> syn::Result<TableHead> {
    let mut head = TableHead::default();
    for attr in attrs {
        if !attr.path().is_ident("table") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
                head.name = Some(meta.value()?.parse::<LitStr>()?.value());
            } else if meta.path.is_ident("rls") {
                head.rls = true;
            } else if meta.path.is_ident("force_rls") {
                head.force_rls = true;
            } else {
                other(&meta)?;
            }
            Ok(())
        })?;
    }
    Ok(head)
}

/// The command a row-level-security policy applies to (`CREATE POLICY … FOR <command>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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

    /// The command a bare attribute flag (`select`, `all`, …) names, if any.
    #[must_use]
    pub fn from_flag(path: &Path) -> Option<Self> {
        [
            Self::All,
            Self::Select,
            Self::Insert,
            Self::Update,
            Self::Delete,
        ]
        .into_iter()
        .find(|command| path.is_ident(command.as_sql()))
    }
}

/// A table privilege grantable to a role (`GRANT <privilege> ON …`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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

    /// The privilege a bare attribute flag (`select`, `all`, …) names, if any.
    #[must_use]
    pub fn from_flag(path: &Path) -> Option<Self> {
        let flag = path.get_ident()?.to_string();
        [
            Self::All,
            Self::Select,
            Self::Insert,
            Self::Update,
            Self::Delete,
        ]
        .into_iter()
        .find(|privilege| privilege.as_sql().split(' ').next() == Some(flag.as_str()))
    }
}

/// One parsed `policy(<name>(<command>, to = "...", using = "...", check = "..."))` entry.
///
/// The policy name is the entry's head identifier (so two policies are
/// distinct nested lists — clippy's `duplicated_attributes` never sees a repeated
/// `to = "role"`).
#[derive(Debug)]
pub struct PolicyEntry {
    /// The policy name (not yet identifier-checked).
    pub name: String,
    /// The command (defaults to `ALL` when omitted).
    pub command: PolicyCommand,
    /// Roles from `to = "..."` (empty = PUBLIC; not yet identifier-checked).
    pub roles: Vec<String>,
    /// The `using` expression (verbatim trusted SQL).
    pub using: Option<String>,
    /// The `check` expression (verbatim trusted SQL).
    pub check: Option<String>,
}

/// Parse one `policy(...)` entry.
///
/// Enforces the rules every reader shares: at most one command, `to` lists at least one role when present, and the
/// command/clause combination Postgres accepts (see [`check_policy_clauses`]).
///
/// # Errors
/// Returns a spanned error for malformed syntax or a rule violation.
pub fn parse_policy_entry(entry: &ParseNestedMeta<'_>) -> syn::Result<PolicyEntry> {
    let name = entry.path.require_ident()?.to_string();
    let mut command = None;
    let mut roles = Vec::new();
    let mut to_present = false;
    let mut using = None;
    let mut check = None;
    entry.parse_nested_meta(|inner| {
        if inner.path.is_ident("to") {
            to_present = true;
            roles = split_roles(&inner.value()?.parse::<LitStr>()?.value());
        } else if inner.path.is_ident("using") {
            using = Some(inner.value()?.parse::<LitStr>()?.value());
        } else if inner.path.is_ident("check") {
            check = Some(inner.value()?.parse::<LitStr>()?.value());
        } else if let Some(parsed) = PolicyCommand::from_flag(&inner.path) {
            if command.is_some() {
                return Err(inner
                    .error("policy has more than one command (all/select/insert/update/delete)"));
            }
            command = Some(parsed);
        } else {
            return Err(inner
                .error("unknown policy entry attribute (expected a command, to, using, check)"));
        }
        Ok(())
    })?;

    // `to = ""` (or all-empty) would silently broaden the policy to PUBLIC — reject
    // it. PUBLIC is reachable only by omitting `to` entirely.
    if to_present && roles.is_empty() {
        return Err(entry.error(format!(
            "policy `{name}`: `to` was specified but lists no roles (omit `to` for PUBLIC)"
        )));
    }
    let command = command.unwrap_or(PolicyCommand::All);
    check_policy_clauses(&name, command, using.as_deref(), check.as_deref())
        .map_err(|message| entry.error(message))?;
    Ok(PolicyEntry {
        name,
        command,
        roles,
        using,
        check,
    })
}

/// Enforce the command/clause rules a policy must satisfy (mirrors Postgres).
///
/// SELECT/DELETE take `using` only, INSERT takes `check` only, ALL/UPDATE take at
/// least one. `using`/`check` are verbatim trusted SQL, so only emptiness is checked.
///
/// # Errors
/// Returns the user-facing message for the first rule the policy breaks.
pub fn check_policy_clauses(
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

/// One parsed `grant(<role>(<privilege>...))` entry.
///
/// The role is the entry's head
/// identifier (lint-safe, like policies), with the privileges as bare flags.
#[derive(Debug)]
pub struct GrantEntry {
    /// The grantee role (not yet identifier-checked).
    pub role: String,
    /// The distinct privileges, in first-seen order (never empty).
    pub privileges: Vec<Privilege>,
}

/// Parse one `grant(...)` entry.
///
/// # Errors
/// Returns a spanned error for an unknown privilege flag or an empty list.
pub fn parse_grant_entry(entry: &ParseNestedMeta<'_>) -> syn::Result<GrantEntry> {
    let role = entry.path.require_ident()?.to_string();
    let mut privileges: Vec<Privilege> = Vec::new();
    entry.parse_nested_meta(|inner| {
        let Some(privilege) = Privilege::from_flag(&inner.path) else {
            return Err(inner.error("expected a privilege (select/insert/update/delete/all)"));
        };
        if !privileges.contains(&privilege) {
            privileges.push(privilege);
        }
        Ok(())
    })?;
    if privileges.is_empty() {
        return Err(entry.error(format!(
            "grant role `{role}` needs at least one privilege (select/insert/update/delete/all)"
        )));
    }
    Ok(GrantEntry { role, privileges })
}

#[cfg(test)]
#[path = "lib_test.rs"]
mod lib_test;
