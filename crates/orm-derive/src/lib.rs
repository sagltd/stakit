//! Derive macros for `stakit-orm`.
//!
//! `#[derive(Table)]` turns a struct into a mapped table: it emits the
//! [`Table`](stakit_orm::Table) impl, typed `Col` column tokens, an `all()`
//! whole-row projection, a sqlx `FromRow` impl, and compile-time foreign-key
//! type checks.

mod types;

use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, Ident, LitInt, LitStr, Path, Type, parse_macro_input};
use types::{is_relation, sql_type, unwrap_generic};

/// Derive [`Table`](stakit_orm::Table) for a struct.
#[proc_macro_derive(Table, attributes(table, column, has_many, belongs_to))]
pub fn derive_table(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(&input)
        .unwrap_or_else(|error| error.to_compile_error())
        .into()
}

/// Derive a named projection: each field carries `#[from(<expr>)]` (a column or
/// aggregate). `T::project()` selects those expressions and decodes into `T`.
#[proc_macro_derive(Row, attributes(from))]
pub fn derive_row(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_row(&input)
        .unwrap_or_else(|error| error.to_compile_error())
        .into()
}

/// Derive `ToValue` + `FromValue` + `IntoExpr` for a fieldless enum so it can be a
/// column type out of the box.
///
/// Default stores the **variant name as text** (`Value::Text`) — portable to every
/// backend (Postgres/MySQL accept a string for native enum columns; SQLite/Turso
/// store it as `TEXT`). `#[db_enum(int)]` stores the **discriminant as an integer**
/// (`Value::I32`), using each variant's explicit `= N` discriminant, a
/// `#[db_enum(value = N)]` override, or the 0-based declaration index. Per-variant
/// `#[db_enum(rename = "...")]` overrides the text label.
///
/// Declare the column's SQL type explicitly, e.g. `#[column(sql_type = "text")]` for
/// a text enum or `#[column(sql_type = "int")]` for a numeric one (or a native enum
/// type name like `"mood"`).
#[proc_macro_derive(DbEnum, attributes(db_enum))]
pub fn derive_db_enum(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_db_enum(&input)
        .unwrap_or_else(|error| error.to_compile_error())
        .into()
}

/// Derive a **Postgres composite type** for a struct so it can be a column field.
///
/// Emits `ToValue`/`FromValue`/`IntoExpr`: the value binds as the composite text
/// literal `(f1,f2,…)` cast `$N::<type_name>`, and reads back from that text form
/// (select the column as `col::text`). The composite's type name defaults to the
/// struct name in `snake_case`; override with `#[db_type(name = "address_type")]`.
/// Use it on a [`Table`](stakit_orm::Table) field with
/// `#[column(sql_type = "address_type")]`. Composite types are **Postgres-only** —
/// binding one on another backend fails at run time with `Error::Unsupported`.
///
/// Each field type must itself be `ToValue + FromValue` (scalars, enums, `Option`).
/// `<Type>::create_type_sql()` returns the `CREATE TYPE … AS (…)` DDL to run before
/// the table migration.
#[proc_macro_derive(Type, attributes(db_type))]
pub fn derive_type(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_type(&input)
        .unwrap_or_else(|error| error.to_compile_error())
        .into()
}

/// Return the first value that appears more than once, if any.
fn first_duplicate(values: &[String]) -> Option<String> {
    let mut seen = std::collections::HashSet::new();
    values.iter().find(|v| !seen.insert(*v)).cloned()
}

#[allow(clippy::too_many_lines)]
fn expand_db_enum(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let name = &input.ident;
    let Data::Enum(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            name,
            "DbEnum can only derive for enums",
        ));
    };

    // Container repr: `#[db_enum(int)]` or `#[db_enum(text)]` (default text).
    let mut as_int = false;
    for attr in &input.attrs {
        if !attr.path().is_ident("db_enum") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("int") {
                as_int = true;
                Ok(())
            } else if meta.path.is_ident("text") {
                as_int = false;
                Ok(())
            } else {
                Err(meta.error("expected `int` or `text`"))
            }
        })?;
    }

    let mut idents = Vec::new();
    let mut texts = Vec::new();
    let mut ints = Vec::new();
    for (index, variant) in data.variants.iter().enumerate() {
        if !matches!(variant.fields, Fields::Unit) {
            return Err(syn::Error::new_spanned(
                &variant.ident,
                "DbEnum variants must be unit (fieldless)",
            ));
        }
        let mut text = variant.ident.to_string();
        // int value: explicit Rust discriminant (`= N`) wins, then declaration index.
        let mut int_value: i64 = i64::try_from(index).unwrap_or(i64::MAX);
        if let Some((_, syn::Expr::Lit(expr_lit))) = &variant.discriminant {
            if let syn::Lit::Int(lit) = &expr_lit.lit {
                int_value = lit.base10_parse()?;
            }
        }
        for attr in &variant.attrs {
            if !attr.path().is_ident("db_enum") {
                continue;
            }
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("rename") {
                    text = meta.value()?.parse::<LitStr>()?.value();
                    Ok(())
                } else if meta.path.is_ident("value") {
                    int_value = meta.value()?.parse::<LitInt>()?.base10_parse()?;
                    Ok(())
                } else {
                    Err(meta.error("expected `rename = \"...\"` or `value = N`"))
                }
            })?;
        }
        idents.push(variant.ident.clone());
        texts.push(text);
        ints.push(int_value);
    }

    if idents.is_empty() {
        return Err(syn::Error::new_spanned(
            name,
            "DbEnum needs at least one variant",
        ));
    }

    // Reject collisions — two variants mapping to the same stored value would make
    // the round-trip lossy (decode picks only the first), silently corrupting data.
    if as_int {
        if let Some(dup) = first_duplicate(&ints.iter().map(i64::to_string).collect::<Vec<_>>()) {
            return Err(syn::Error::new_spanned(
                name,
                format!("DbEnum has two variants with the same int value `{dup}`"),
            ));
        }
    } else if let Some(dup) = first_duplicate(&texts) {
        return Err(syn::Error::new_spanned(
            name,
            format!("DbEnum has two variants with the same text label {dup:?}"),
        ));
    }

    let body = if as_int {
        let mut lits = Vec::with_capacity(ints.len());
        for value in &ints {
            let narrowed = i32::try_from(*value).map_err(|_| {
                syn::Error::new_spanned(name, format!("DbEnum int value {value} out of i32 range"))
            })?;
            lits.push(proc_macro2::Literal::i32_unsuffixed(narrowed));
        }
        quote! {
            #[automatically_derived]
            impl ::stakit_orm::ToValue for #name {
                fn to_value(self) -> ::stakit_orm::Value {
                    ::stakit_orm::Value::I32(match self { #( Self::#idents => #lits ),* })
                }
            }
            #[automatically_derived]
            impl ::stakit_orm::FromValue for #name {
                const KIND: ::stakit_orm::ValueKind = ::stakit_orm::ValueKind::I32;
                fn from_value(value: ::stakit_orm::Value) -> ::stakit_orm::Result<Self> {
                    match value {
                        ::stakit_orm::Value::I32(n) => match n {
                            #( #lits => ::core::result::Result::Ok(Self::#idents), )*
                            other => ::core::result::Result::Err(::stakit_orm::Error::Decode(
                                ::std::format!(concat!("invalid ", stringify!(#name), ": {}"), other).into(),
                            )),
                        },
                        other => ::core::result::Result::Err(::stakit_orm::Error::Decode(
                            ::std::format!(concat!("expected int for ", stringify!(#name), ", got {:?}"), other).into(),
                        )),
                    }
                }
            }
        }
    } else {
        quote! {
            #[automatically_derived]
            impl ::stakit_orm::ToValue for #name {
                fn to_value(self) -> ::stakit_orm::Value {
                    ::stakit_orm::Value::Text(match self { #( Self::#idents => #texts ),* }.to_owned())
                }
            }
            #[automatically_derived]
            impl ::stakit_orm::FromValue for #name {
                const KIND: ::stakit_orm::ValueKind = ::stakit_orm::ValueKind::Text;
                fn from_value(value: ::stakit_orm::Value) -> ::stakit_orm::Result<Self> {
                    match value {
                        ::stakit_orm::Value::Text(s) => match s.as_str() {
                            #( #texts => ::core::result::Result::Ok(Self::#idents), )*
                            other => ::core::result::Result::Err(::stakit_orm::Error::Decode(
                                ::std::format!(concat!("invalid ", stringify!(#name), ": {:?}"), other).into(),
                            )),
                        },
                        other => ::core::result::Result::Err(::stakit_orm::Error::Decode(
                            ::std::format!(concat!("expected text for ", stringify!(#name), ", got {:?}"), other).into(),
                        )),
                    }
                }
            }
        }
    };

    Ok(quote! {
        #body

        #[automatically_derived]
        impl ::stakit_orm::expr::IntoExpr<#name> for #name {
            fn into_operand(self) -> ::stakit_orm::expr::Operand {
                ::stakit_orm::expr::Operand::Value(<Self as ::stakit_orm::ToValue>::to_value(self))
            }
        }
    })
}

/// Convert a `CamelCase` ident to `snake_case` (default composite type name).
fn to_snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (index, ch) in name.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if index > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

#[allow(clippy::too_many_lines)]
fn expand_type(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let name = &input.ident;
    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            name,
            "Type can only derive for structs",
        ));
    };
    let Fields::Named(fields) = &data.fields else {
        return Err(syn::Error::new_spanned(
            name,
            "Type requires named fields (a Postgres composite type)",
        ));
    };
    if fields.named.is_empty() {
        return Err(syn::Error::new_spanned(
            name,
            "Type needs at least one field",
        ));
    }

    // Composite type name: `#[db_type(name = "address_type")]` or snake_case struct.
    let mut type_name = to_snake_case(&name.to_string());
    for attr in &input.attrs {
        if !attr.path().is_ident("db_type") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
                type_name = meta.value()?.parse::<LitStr>()?.value();
                Ok(())
            } else {
                Err(meta.error("expected `name = \"...\"`"))
            }
        })?;
    }

    let mut field_idents = Vec::new();
    let mut field_types = Vec::new();
    let mut ddl_fields = Vec::new(); // "field sqltype" pairs for CREATE TYPE
    for field in &fields.named {
        let ident = field.ident.clone().expect("named field");
        let ty = &field.ty;

        // Per-field SQL type: `#[db_type(sql_type = "...")]` or inferred (Option<T>
        // maps via its inner T). Used only for `create_type_sql()`.
        let mut explicit_sql: Option<String> = None;
        for attr in &field.attrs {
            if !attr.path().is_ident("db_type") {
                continue;
            }
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("sql_type") {
                    explicit_sql = Some(meta.value()?.parse::<LitStr>()?.value());
                    Ok(())
                } else {
                    Err(meta.error("expected `sql_type = \"...\"`"))
                }
            })?;
        }
        let base_ty = unwrap_generic(ty, "Option").unwrap_or(ty);
        let sql = match explicit_sql {
            Some(s) => s,
            None => sql_type(base_ty)
                .ok_or_else(|| {
                    syn::Error::new_spanned(
                        ty,
                        "composite field type needs `#[db_type(sql_type = \"...\")]` (unknown SQL mapping)",
                    )
                })?
                .to_owned(),
        };
        // `bytea` has no faithful composite *text* encoding here — reject it rather
        // than silently dropping the bytes (use a hex `text` field or `Json` instead).
        if sql.eq_ignore_ascii_case("bytea") {
            return Err(syn::Error::new_spanned(
                ty,
                "bytea fields are not supported in #[derive(Type)] composites; \
                 store bytes as hex text or in a Json field",
            ));
        }
        let field_name = ident.to_string();
        ddl_fields.push(format!("{field_name} {sql}"));
        field_idents.push(ident);
        field_types.push(ty.clone());
    }

    let field_count = field_idents.len();
    let create_type_sql = format!("create type {type_name} as ({})", ddl_fields.join(", "));

    Ok(quote! {
        #[automatically_derived]
        impl ::stakit_orm::ToValue for #name {
            // The `::<type_name>` cast is applied at the bind boundary (so it also
            // casts a NULL `Option<Self>`); `to_value` yields the plain text literal.
            const WRITE_CAST: ::core::option::Option<&'static str> =
                ::core::option::Option::Some(#type_name);
            fn to_value(self) -> ::stakit_orm::Value {
                let __fields = [
                    #( ::stakit_orm::ToValue::to_value(self.#field_idents) ),*
                ];
                ::stakit_orm::Value::Text(::stakit_orm::composite::encode(&__fields))
            }
        }

        #[automatically_derived]
        impl ::stakit_orm::FromValue for #name {
            const KIND: ::stakit_orm::ValueKind = ::stakit_orm::ValueKind::Text;
            // Select the composite as `col::text` so it arrives as the text literal.
            const READ_CAST: ::core::option::Option<&'static str> = ::core::option::Option::Some("text");
            fn from_value(value: ::stakit_orm::Value) -> ::stakit_orm::Result<Self> {
                let __text = match value {
                    ::stakit_orm::Value::Text(s) => s,
                    ::stakit_orm::Value::Cast { inner, .. } => match *inner {
                        ::stakit_orm::Value::Text(s) => s,
                        other => return ::core::result::Result::Err(::stakit_orm::Error::Decode(
                            ::std::format!(concat!("expected composite text for ", stringify!(#name), ", got {:?}"), other).into(),
                        )),
                    },
                    other => return ::core::result::Result::Err(::stakit_orm::Error::Decode(
                        ::std::format!(concat!("expected composite text for ", stringify!(#name), ", got {:?}"), other).into(),
                    )),
                };
                let mut __fields = ::stakit_orm::composite::parse(&__text, #field_count)?.into_iter();
                ::core::result::Result::Ok(Self {
                    #( #field_idents: {
                        let __f = __fields.next().ok_or_else(|| ::stakit_orm::Error::Decode(
                            "composite: missing field".into(),
                        ))?;
                        <#field_types as ::stakit_orm::FromValue>::from_value(
                            ::stakit_orm::composite::field_value(
                                &__f,
                                <#field_types as ::stakit_orm::FromValue>::KIND,
                            )?,
                        )?
                    } ),*
                })
            }
        }

        #[automatically_derived]
        impl ::stakit_orm::expr::IntoExpr<#name> for #name {
            fn into_operand(self) -> ::stakit_orm::expr::Operand {
                // Apply the composite cast so `eq(col, value)` renders `$N::<type>`.
                ::stakit_orm::expr::Operand::Value(::stakit_orm::value::with_cast(
                    <Self as ::stakit_orm::ToValue>::to_value(self),
                    <Self as ::stakit_orm::ToValue>::WRITE_CAST,
                ))
            }
        }

        #[automatically_derived]
        impl #name {
            /// The Postgres composite type name (`#[db_type(name=…)]` or snake_case).
            pub const SQL_TYPE_NAME: &'static str = #type_name;

            /// The `CREATE TYPE … AS (…)` DDL to run before any table that uses it.
            #[must_use]
            pub fn create_type_sql() -> &'static str {
                #create_type_sql
            }
        }
    })
}

fn expand_row(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let name = &input.ident;
    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            name,
            "Row can only derive for structs",
        ));
    };
    let Fields::Named(fields) = &data.fields else {
        return Err(syn::Error::new_spanned(name, "Row requires named fields"));
    };

    let mut idents = Vec::new();
    let mut exprs = Vec::new();
    for field in &fields.named {
        let ident = field.ident.clone().expect("named field");
        let from = field
            .attrs
            .iter()
            .find(|attr| attr.path().is_ident("from"))
            .ok_or_else(|| syn::Error::new_spanned(&ident, "Row field needs #[from(<expr>)]"))?;
        let expr: proc_macro2::TokenStream = from.parse_args()?;
        idents.push(ident);
        exprs.push(expr);
    }

    let proj = quote::format_ident!("{}Projection", name);
    let write = exprs.iter().enumerate().map(|(index, expr)| {
        let separator = if index > 0 {
            quote! { out.push(", "); }
        } else {
            quote! {}
        };
        quote! {
            #separator
            ::stakit_orm::Projection::write_columns(&(#expr), out)?;
        }
    });
    let decode = idents.iter().zip(&exprs).map(|(ident, expr)| {
        quote! {
            #ident: {
                let __value = ::stakit_orm::Projection::decode(&(#expr), row, __offset)?;
                __offset += ::stakit_orm::Projection::arity(&(#expr));
                __value
            }
        }
    });
    let arities = exprs
        .iter()
        .map(|expr| quote! { + ::stakit_orm::Projection::arity(&(#expr)) });

    let vis = &input.vis;
    Ok(quote! {
        #[doc = concat!("Projection for [`", stringify!(#name), "`] (from `#[derive(Row)]`).")]
        #[derive(Clone, Copy)]
        #vis struct #proj;

        impl ::stakit_orm::Projection for #proj {
            type Output = #name;
            fn arity(&self) -> usize {
                0 #(#arities)*
            }
            fn write_columns(&self, out: &mut ::stakit_orm::SqlWriter) -> ::stakit_orm::Result<()> {
                #(#write)*
                Ok(())
            }
            #[allow(unused_assignments)]
            fn decode(
                &self,
                row: &dyn ::stakit_orm::driver::Row,
                start: usize,
            ) -> ::stakit_orm::Result<#name> {
                let mut __offset = start;
                Ok(#name { #(#decode),* })
            }
        }

        impl #name {
            /// The projection selecting this row's `#[from(..)]` expressions.
            #[must_use]
            #vis fn project() -> #proj {
                #proj
            }
        }
    })
}

#[allow(clippy::struct_excessive_bools)] // column flags (pk/unique/index/nullable)
struct ColumnModel {
    field: Ident,
    col_name: String,
    field_ty: Type,
    sql_type: String,
    is_pk: bool,
    is_unique: bool,
    is_index: bool,
    index_method: Option<String>,
    opclass: Option<String>,
    is_nullable: bool,
    default: Option<String>,
    references: Option<(Path, String)>,
    on_delete: Option<String>,
    generated: Option<String>,
}

fn expand(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let name = &input.ident;
    let table_name = table_name(input)?;
    validate_ident(&table_name, name)?;

    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            name,
            "Table can only derive for structs",
        ));
    };
    let Fields::Named(fields) = &data.fields else {
        return Err(syn::Error::new_spanned(name, "Table requires named fields"));
    };

    let mut columns = Vec::new();
    let mut relations = Vec::new();
    for field in &fields.named {
        let ident = field.ident.clone().expect("named field");
        if is_relation(&field.ty) {
            relations.push(ident);
            continue;
        }
        columns.push(parse_column(&ident, field)?);
    }

    // A `GENERATED ALWAYS AS … STORED` column is database-computed, not stored data:
    // it is excluded from `COLUMNS` (so whole-row `SELECT`/decode skips it — a Postgres
    // `tsvector`, say, has no scalar decode), while still getting a `Col` token so it can
    // be referenced in predicates such as `matches_tsv`. Its struct field is filled with
    // `Default` on read, like a relation.
    let stored: Vec<&ColumnModel> = columns.iter().filter(|c| c.generated.is_none()).collect();
    let generated: Vec<&ColumnModel> = columns.iter().filter(|c| c.generated.is_some()).collect();

    let column_literals = stored.iter().map(|c| column_literal(c));
    let col_consts = columns.iter().map(|column| {
        let field = &column.field;
        let field_ty = &column.field_ty;
        let col_name = &column.col_name;
        quote! {
            #[allow(non_upper_case_globals)]
            pub const #field: ::stakit_orm::Col<Self, #field_ty> =
                ::stakit_orm::Col::new(#table_name, #col_name);
        }
    });
    let from_row_at_fields = stored.iter().enumerate().map(|(index, column)| {
        let field = &column.field;
        let field_ty = &column.field_ty;
        quote! {
            #field: ::stakit_orm::driver::decode_cell::<#field_ty>(row, start + #index)?
        }
    });
    let from_row_at_generated = generated.iter().map(|column| {
        let field = &column.field;
        quote! { #field: ::core::default::Default::default() }
    });
    let from_row_at_relations = relations.iter().map(|ident| {
        quote! { #ident: ::core::default::Default::default() }
    });
    let fk_checks = columns.iter().filter_map(fk_check);

    // `type Pk` mirrors the primary key's shape: `()` when there is none, the field
    // type for a single key, or a tuple for a composite key. A tuple `Pk` has no
    // `ToValue` impl, so `Db::get::<T>(..)` won't compile for a composite-key table —
    // those are queried with `find().filter(..)`, and a scalar FK can't reference one
    // (the FK type-equality check rejects the tuple). The DDL emits `primary key (a, b)`.
    let pk_columns: Vec<&ColumnModel> = stored
        .iter()
        .copied()
        .filter(|column| column.is_pk)
        .collect();
    let pk_ty = match pk_columns.as_slice() {
        [] => quote! { () },
        [single] => single.field_ty.to_token_stream(),
        many => {
            let types = many.iter().map(|column| &column.field_ty);
            quote! { ( #(#types),* ) }
        }
    };

    let insertable = emit_insertable(name, &table_name, &columns);

    Ok(quote! {
        impl ::stakit_orm::Table for #name {
            const TABLE: &'static str = #table_name;
            const COLUMNS: &'static [::stakit_orm::Column] = &[ #(#column_literals),* ];
            type Pk = #pk_ty;

            fn from_row_at(
                row: &dyn ::stakit_orm::driver::Row,
                start: usize,
            ) -> ::stakit_orm::Result<Self> {
                Ok(Self {
                    #(#from_row_at_fields,)*
                    #(#from_row_at_generated,)*
                    #(#from_row_at_relations,)*
                })
            }
        }

        impl #name {
            #(#col_consts)*
            /// Whole-row projection (`SELECT` every column).
            #[must_use]
            pub fn all() -> ::stakit_orm::All<Self> {
                ::stakit_orm::All::new()
            }
        }

        #(#fk_checks)*

        #insertable
    })
}

/// Emit the `…New` insert companion struct and its `Insertable` impl.
fn emit_insertable(name: &Ident, table: &str, columns: &[ColumnModel]) -> proc_macro2::TokenStream {
    let new_ident = quote::format_ident!("{}New", name);

    // A `GENERATED ALWAYS AS … STORED` column is computed by the database and cannot
    // be written, so it is omitted from the insert companion entirely.
    let columns: Vec<&ColumnModel> = columns.iter().filter(|c| c.generated.is_none()).collect();

    let new_fields = columns.iter().map(|column| {
        let field = &column.field;
        let ty = &column.field_ty;
        if column.default.is_some() {
            let base = unwrap_generic(ty, "Option").unwrap_or(ty);
            quote! { pub #field: ::core::option::Option<#base> }
        } else {
            quote! { pub #field: #ty }
        }
    });

    let required: Vec<&ColumnModel> = columns
        .iter()
        .copied()
        .filter(|c| c.default.is_none())
        .collect();
    let optional: Vec<&ColumnModel> = columns
        .iter()
        .copied()
        .filter(|c| c.default.is_some())
        .collect();

    let required_names = required.iter().map(|c| &c.col_name);
    let optional_names = optional.iter().map(|c| &c.col_name);
    let present = optional.iter().map(|c| {
        let field = &c.field;
        quote! { __present.push(self.#field.is_some()); }
    });
    let required_binds = required.iter().map(|c| {
        let field = &c.field;
        quote! {
            if !__first { writer.push(", "); }
            __first = false;
            writer.push_bind(::stakit_orm::insert::boxed_bind(self.#field));
        }
    });
    let optional_binds = optional.iter().enumerate().map(|(index, c)| {
        let field = &c.field;
        quote! {
            if optional_included[#index] {
                if !__first { writer.push(", "); }
                __first = false;
                writer.push_bind(::stakit_orm::insert::boxed_bind(self.#field));
            }
        }
    });

    quote! {
        #[doc = concat!("Insert companion for [`", stringify!(#name), "`]: defaulted columns are `Option` (omit to use the DB default).")]
        #[allow(missing_docs)]
        pub struct #new_ident {
            #(#new_fields,)*
        }

        impl ::stakit_orm::Insertable for #new_ident {
            const TABLE: &'static str = #table;
            const REQUIRED: &'static [&'static str] = &[ #(#required_names),* ];
            const OPTIONAL: &'static [&'static str] = &[ #(#optional_names),* ];

            fn optional_present(&self) -> ::stakit_orm::OptionalPresent {
                let mut __present = ::stakit_orm::OptionalPresent::new();
                #(#present)*
                __present
            }

            fn bind_values(self, optional_included: &[bool], writer: &mut ::stakit_orm::SqlWriter) {
                let mut __first = true;
                #(#required_binds)*
                #(#optional_binds)*
                let _ = __first;
            }
        }
    }
}

fn table_name(input: &DeriveInput) -> syn::Result<String> {
    for attr in &input.attrs {
        if !attr.path().is_ident("table") {
            continue;
        }
        let mut found = None;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
                found = Some(meta.value()?.parse::<LitStr>()?.value());
            }
            Ok(())
        })?;
        if let Some(name) = found {
            return Ok(name);
        }
    }
    Err(syn::Error::new_spanned(
        input,
        "missing #[table(name = \"...\")]",
    ))
}

fn parse_column(ident: &Ident, field: &syn::Field) -> syn::Result<ColumnModel> {
    let mut model = ColumnModel {
        field: ident.clone(),
        col_name: ident.to_string(),
        field_ty: field.ty.clone(),
        sql_type: String::new(),
        is_pk: false,
        is_unique: false,
        is_index: false,
        index_method: None,
        opclass: None,
        is_nullable: unwrap_generic(&field.ty, "Option").is_some(),
        default: None,
        references: None,
        on_delete: None,
        generated: None,
    };
    let mut explicit_sql_type = None;
    for attr in &field.attrs {
        if !attr.path().is_ident("column") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("pk") {
                model.is_pk = true;
            } else if meta.path.is_ident("unique") {
                model.is_unique = true;
            } else if meta.path.is_ident("index") {
                model.is_index = true;
                // `#[column(index)]` is the bare B-tree form; `#[column(index = "gist")]`
                // requests an explicit access method (e.g. GiST for PostGIS columns).
                if let Ok(value) = meta.value() {
                    model.index_method = Some(value.parse::<LitStr>()?.value());
                }
            } else if meta.path.is_ident("index_method") {
                // Keyword spelling of the access method (e.g. `index_method = "hnsw"`
                // for pgvector); equivalent to `index = "hnsw"`.
                model.index_method = Some(meta.value()?.parse::<LitStr>()?.value());
            } else if meta.path.is_ident("opclass") {
                // Operator class on the indexed column (e.g. `vector_cosine_ops`).
                model.opclass = Some(meta.value()?.parse::<LitStr>()?.value());
            } else if meta.path.is_ident("nullable") {
                model.is_nullable = true;
            } else if meta.path.is_ident("name") {
                model.col_name = meta.value()?.parse::<LitStr>()?.value();
            } else if meta.path.is_ident("default") {
                model.default = Some(meta.value()?.parse::<LitStr>()?.value());
            } else if meta.path.is_ident("generated") {
                // A `GENERATED ALWAYS AS (<expr>) STORED` column (e.g. a stored
                // tsvector). The database computes it, so it is omitted from inserts.
                model.generated = Some(meta.value()?.parse::<LitStr>()?.value());
            } else if meta.path.is_ident("sql_type") {
                explicit_sql_type = Some(meta.value()?.parse::<LitStr>()?.value());
            } else if meta.path.is_ident("references") {
                model.references = Some(parse_references(&meta)?);
            } else if meta.path.is_ident("on_delete") {
                model.on_delete = Some(meta.value()?.parse::<LitStr>()?.value());
            } else {
                // Reject unknown keys rather than silently dropping them — a typo like
                // `index_methd = "hnsw"` must fail loudly, not produce a B-tree.
                return Err(meta.error("unknown #[column(...)] attribute"));
            }
            Ok(())
        })?;
    }

    validate_ident(&model.col_name, ident)?;
    validate_column(&model, ident)?;

    let base_ty = unwrap_generic(&field.ty, "Option").unwrap_or(&field.ty);
    model.sql_type = match explicit_sql_type {
        Some(explicit) => explicit,
        None => sql_type(base_ty)
            .ok_or_else(|| {
                syn::Error::new_spanned(
                    base_ty,
                    "unknown SQL type for this Rust type; add #[column(sql_type = \"...\")]",
                )
            })?
            .to_owned(),
    };
    Ok(model)
}

/// Reject contradictory `#[column(...)]` flag combinations at expansion time.
fn validate_column(model: &ColumnModel, ident: &Ident) -> syn::Result<()> {
    // on_delete is only valid with references, must be a known keyword, and
    // `set null` requires a nullable column (matches the spec's compile checks).
    if let Some(action) = &model.on_delete {
        if model.references.is_none() {
            return Err(syn::Error::new_spanned(
                ident,
                "on_delete requires references",
            ));
        }
        if !matches!(
            action.as_str(),
            "cascade" | "restrict" | "set null" | "no action"
        ) {
            return Err(syn::Error::new_spanned(
                ident,
                "on_delete must be one of: cascade, restrict, set null, no action",
            ));
        }
        if action == "set null" && !model.is_nullable {
            return Err(syn::Error::new_spanned(
                ident,
                "on_delete = \"set null\" requires a nullable (Option<_>) column",
            ));
        }
    }

    // An access method / operator class only means something on an indexed column.
    if !model.is_index && (model.index_method.is_some() || model.opclass.is_some()) {
        return Err(syn::Error::new_spanned(
            ident,
            "index_method/opclass require #[column(index)] on the same column",
        ));
    }

    // The access method and operator class are written verbatim into `CREATE INDEX …
    // USING <method> (col <opclass>)`. They are SQL *identifiers* (e.g. `hnsw`,
    // `vector_cosine_ops`), so require them to be bare identifiers — this keeps a stray
    // value from breaking out of the index clause.
    if let Some(method) = &model.index_method {
        if !is_sql_identifier(method, false) {
            return Err(syn::Error::new_spanned(
                ident,
                "index_method must be a bare SQL identifier (e.g. \"hnsw\", \"gin\")",
            ));
        }
    }
    if let Some(opclass) = &model.opclass {
        if !is_sql_identifier(opclass, true) {
            return Err(syn::Error::new_spanned(
                ident,
                "opclass must be a (optionally schema-qualified) SQL identifier",
            ));
        }
    }

    // A column is either database-generated or has a default — never both.
    if model.generated.is_some() && model.default.is_some() {
        return Err(syn::Error::new_spanned(
            ident,
            "a #[column(generated = ...)] column cannot also have a default",
        ));
    }

    Ok(())
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

/// Reject identifiers Postgres cannot store safely (matches `ident::validate`).
fn validate_ident(name: &str, span: &Ident) -> syn::Result<()> {
    if name.is_empty() {
        return Err(syn::Error::new_spanned(span, "identifier is empty"));
    }
    if name.as_bytes().contains(&0) {
        return Err(syn::Error::new_spanned(
            span,
            "identifier contains a NUL byte",
        ));
    }
    if name.len() > 63 {
        return Err(syn::Error::new_spanned(
            span,
            "identifier exceeds Postgres NAMEDATALEN (63 bytes)",
        ));
    }
    Ok(())
}

fn parse_references(meta: &syn::meta::ParseNestedMeta<'_>) -> syn::Result<(Path, String)> {
    let path: Path = meta.value()?.parse()?;
    let column = path
        .segments
        .last()
        .map(|segment| segment.ident.to_string())
        .ok_or_else(|| syn::Error::new_spanned(&path, "empty references path"))?;
    let mut root = path;
    root.segments.pop();
    if let Some(pair) = root.segments.pop() {
        root.segments.push_value(pair.into_value());
    }
    Ok((root, column))
}

fn on_delete_tokens(value: Option<&str>) -> proc_macro2::TokenStream {
    match value {
        Some("cascade") => quote! { ::stakit_orm::OnDelete::Cascade },
        Some("restrict") => quote! { ::stakit_orm::OnDelete::Restrict },
        Some("set null") => quote! { ::stakit_orm::OnDelete::SetNull },
        _ => quote! { ::stakit_orm::OnDelete::NoAction },
    }
}

fn column_literal(column: &ColumnModel) -> proc_macro2::TokenStream {
    let name = &column.col_name;
    let sql_type = &column.sql_type;
    let is_pk = column.is_pk;
    let is_unique = column.is_unique;
    let is_index = column.is_index;
    let index_method = column.index_method.as_ref().map_or_else(
        || quote! { ::core::option::Option::None },
        |method| quote! { ::core::option::Option::Some(#method) },
    );
    let index_opclass = column.opclass.as_ref().map_or_else(
        || quote! { ::core::option::Option::None },
        |opclass| quote! { ::core::option::Option::Some(#opclass) },
    );
    let is_nullable = column.is_nullable;
    let default = column.default.as_ref().map_or_else(
        || quote! { ::core::option::Option::None },
        |value| quote! { ::core::option::Option::Some(#value) },
    );
    let on_delete = on_delete_tokens(column.on_delete.as_deref());
    let references = column.references.as_ref().map_or_else(
        || quote! { ::core::option::Option::None },
        |(root, ref_column)| {
            quote! {
                ::core::option::Option::Some(::stakit_orm::ForeignKey {
                    table: <#root as ::stakit_orm::Table>::TABLE,
                    column: #ref_column,
                    on_delete: #on_delete,
                })
            }
        },
    );
    let field_ty = &column.field_ty;
    quote! {
        ::stakit_orm::Column {
            name: #name,
            sql_type: #sql_type,
            is_pk: #is_pk,
            is_unique: #is_unique,
            is_index: #is_index,
            index_method: #index_method,
            index_opclass: #index_opclass,
            is_nullable: #is_nullable,
            default: #default,
            references: #references,
            // Read-side cast (e.g. composite `col::text`), from the field's type.
            read_cast: <#field_ty as ::stakit_orm::FromValue>::READ_CAST,
        }
    }
}

fn fk_check(column: &ColumnModel) -> Option<proc_macro2::TokenStream> {
    let (root, _) = column.references.as_ref()?;
    let field_ty = unwrap_generic(&column.field_ty, "Option").unwrap_or(&column.field_ty);
    Some(quote! {
        const _: fn() = || {
            fn assert_same<T>(_: ::core::marker::PhantomData<T>, _: ::core::marker::PhantomData<T>) {}
            assert_same(
                ::core::marker::PhantomData::<#field_ty>,
                ::core::marker::PhantomData::<<#root as ::stakit_orm::Table>::Pk>,
            );
        };
    })
}

use quote::ToTokens;
