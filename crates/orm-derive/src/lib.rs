//! Derive macros for `stakit-orm`.
//!
//! `#[derive(Table)]` turns a struct into a mapped table: it emits the
//! [`Table`](stakit_orm::Table) impl, typed `Col` column tokens, an `all()`
//! whole-row projection, a sqlx `FromRow` impl, and compile-time foreign-key
//! type checks.

mod types;

use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, Ident, LitStr, Path, Type, parse_macro_input};
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

struct ColumnModel {
    field: Ident,
    col_name: String,
    field_ty: Type,
    sql_type: String,
    is_pk: bool,
    is_unique: bool,
    is_nullable: bool,
    default: Option<String>,
    references: Option<(Path, String)>,
    on_delete: Option<String>,
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

    // A single-column primary key is assumed by `type Pk`, `get()`/`pk_filter`, and
    // the FK type-equality check. Reject composite PKs up front rather than silently
    // filtering on only the first key column.
    if columns.iter().filter(|column| column.is_pk).count() > 1 {
        return Err(syn::Error::new_spanned(
            name,
            "composite primary keys are not supported: mark exactly one field \
             #[column(pk)]",
        ));
    }

    let column_literals = columns.iter().map(column_literal);
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
    let from_row_at_fields = columns.iter().enumerate().map(|(index, column)| {
        let field = &column.field;
        let field_ty = &column.field_ty;
        quote! {
            #field: ::stakit_orm::driver::decode_cell::<#field_ty>(row, start + #index)?
        }
    });
    let from_row_at_relations = relations.iter().map(|ident| {
        quote! { #ident: ::core::default::Default::default() }
    });
    let fk_checks = columns.iter().filter_map(fk_check);

    let pk_ty = columns
        .iter()
        .find(|column| column.is_pk)
        .map_or_else(|| quote! { () }, |column| column.field_ty.to_token_stream());

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

    let required: Vec<&ColumnModel> = columns.iter().filter(|c| c.default.is_none()).collect();
    let optional: Vec<&ColumnModel> = columns.iter().filter(|c| c.default.is_some()).collect();

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
        is_nullable: unwrap_generic(&field.ty, "Option").is_some(),
        default: None,
        references: None,
        on_delete: None,
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
            } else if meta.path.is_ident("nullable") {
                model.is_nullable = true;
            } else if meta.path.is_ident("name") {
                model.col_name = meta.value()?.parse::<LitStr>()?.value();
            } else if meta.path.is_ident("default") {
                model.default = Some(meta.value()?.parse::<LitStr>()?.value());
            } else if meta.path.is_ident("sql_type") {
                explicit_sql_type = Some(meta.value()?.parse::<LitStr>()?.value());
            } else if meta.path.is_ident("references") {
                model.references = Some(parse_references(&meta)?);
            } else if meta.path.is_ident("on_delete") {
                model.on_delete = Some(meta.value()?.parse::<LitStr>()?.value());
            }
            Ok(())
        })?;
    }

    validate_ident(&model.col_name, ident)?;

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
    quote! {
        ::stakit_orm::Column {
            name: #name,
            sql_type: #sql_type,
            is_pk: #is_pk,
            is_unique: #is_unique,
            is_nullable: #is_nullable,
            default: #default,
            references: #references,
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
