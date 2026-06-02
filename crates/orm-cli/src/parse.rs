//! Parse `#[derive(Table)]` structs from a Rust source file into a [`Schema`]
//! using `syn` only — no compilation, no database.

use crate::model::{Column, ForeignKey, Schema, Table};
use std::collections::HashMap;
use syn::{Attribute, Data, DeriveInput, Fields, GenericArgument, Item, PathArguments, Type};

/// Parse all tables from Rust source text.
///
/// # Errors
/// Returns a message if the source cannot be parsed or a column type is unknown.
pub fn parse_schema(source: &str) -> Result<Schema, String> {
    let file = syn::parse_file(source).map_err(|error| error.to_string())?;

    // Pass 1: collect (struct ident -> table name) so FK paths resolve.
    let mut table_of_ident: HashMap<String, String> = HashMap::new();
    let mut inputs: Vec<DeriveInput> = Vec::new();
    for item in file.items {
        let Item::Struct(item_struct) = item else {
            continue;
        };
        let input: DeriveInput = item_struct.into();
        if !derives_table(&input.attrs) {
            continue;
        }
        let table_name = table_name(&input)?;
        table_of_ident.insert(input.ident.to_string(), table_name);
        inputs.push(input);
    }

    // Pass 2: build columns, resolving FK target tables.
    let mut tables = Vec::new();
    for input in &inputs {
        tables.push(parse_table(input, &table_of_ident)?);
    }
    tables.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Schema { tables })
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
        "NaiveDate" => "date",
        "Value" => "jsonb",
        _ => return None,
    };
    Some(mapped.to_owned())
}

#[cfg(test)]
mod tests {
    use super::parse_schema;

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
