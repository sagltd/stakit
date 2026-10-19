use super::*;
use syn::parse_quote;

#[test]
fn sql_type_maps_known_spellings_and_rejects_others() {
    let uuid: Type = parse_quote!(uuid::Uuid);
    let bytes: Type = parse_quote!(Vec<u8>);
    let ints: Type = parse_quote!(Vec<i32>);
    let custom: Type = parse_quote!(MyType);
    assert_eq!(sql_type(&uuid), Some("uuid"));
    assert_eq!(sql_type(&bytes), Some("bytea"));
    assert_eq!(sql_type(&ints), None);
    assert_eq!(sql_type(&custom), None);
}

#[test]
fn split_table_column_requires_exactly_one_dot() {
    assert_eq!(
        split_table_column("users.id"),
        Ok(("users".to_owned(), "id".to_owned()))
    );
    assert!(split_table_column("users").is_err());
    assert!(split_table_column("a.b.c").is_err());
}

#[test]
fn flags_map_to_commands_and_privileges() {
    let select: Path = parse_quote!(select);
    let all: Path = parse_quote!(all);
    let other: Path = parse_quote!(truncate);
    assert_eq!(
        PolicyCommand::from_flag(&select),
        Some(PolicyCommand::Select)
    );
    assert_eq!(Privilege::from_flag(&all), Some(Privilege::All));
    assert_eq!(Privilege::from_flag(&other), None);
}

/// Parse a `#[table(...)]` attribute, collecting its policies and grants.
fn parse(attr: Attribute) -> syn::Result<(TableHead, Vec<PolicyEntry>, Vec<GrantEntry>)> {
    let mut policies = Vec::new();
    let mut grants = Vec::new();
    let head = parse_table_attrs(&[attr], |meta| {
        if meta.path.is_ident("policy") {
            meta.parse_nested_meta(|entry| {
                policies.push(parse_policy_entry(&entry)?);
                Ok(())
            })
        } else if meta.path.is_ident("grant") {
            meta.parse_nested_meta(|entry| {
                grants.push(parse_grant_entry(&entry)?);
                Ok(())
            })
        } else {
            Err(meta.error("unknown"))
        }
    })?;
    Ok((head, policies, grants))
}

#[test]
fn table_attrs_parse_head_policies_and_grants() {
    let (head, policies, grants) = parse(parse_quote!(#[table(
        name = "posts",
        rls,
        policy(own(select, to = "app, admin", using = "true")),
        grant(app(select, insert, select))
    )]))
    .unwrap();
    assert_eq!(head.name.as_deref(), Some("posts"));
    assert!(head.rls && !head.force_rls);
    assert_eq!(policies[0].command, PolicyCommand::Select);
    assert_eq!(policies[0].roles, ["app", "admin"]);
    assert_eq!(grants[0].privileges, [Privilege::Select, Privilege::Insert]);
}

#[test]
fn policy_and_grant_rules_are_enforced() {
    let bad: [Attribute; 7] = [
        parse_quote!(#[table(policy(p(select, check = "x", using = "y")))]),
        parse_quote!(#[table(policy(p(insert, using = "x")))]),
        parse_quote!(#[table(policy(p(all)))]),
        parse_quote!(#[table(policy(p(select, update, using = "x")))]),
        parse_quote!(#[table(policy(p(to = "", using = "x")))]),
        parse_quote!(#[table(grant(app()))]),
        parse_quote!(#[table(typo)]),
    ];
    for attr in bad {
        assert!(parse(attr).is_err());
    }
}
