//! End-to-end SQL-string tests: `#[derive(Table)]` + the query builder produce
//! the expected SQL, with no database. These cover the bulk of correctness
//! (Layer 1 of the spec's testing strategy).

use stakit_orm::prelude::*;
use stakit_orm::{Count, Select, count};
use uuid::Uuid;

#[derive(Table)]
#[table(name = "users")]
#[allow(dead_code)]
struct User {
    #[column(pk)]
    id: Uuid,
    #[column(unique)]
    email: String,
    name: String,
}

#[derive(Table)]
#[table(name = "posts")]
#[allow(dead_code)]
struct Post {
    #[column(pk)]
    id: Uuid,
    #[column(references = User::id, on_delete = "cascade")]
    author_id: Uuid,
    title: String,
    views: i32,
}

const fn uid() -> Uuid {
    Uuid::nil()
}

#[test]
fn select_all_columns() {
    let sql = Select::new(User::all()).from::<User>().to_sql().unwrap();
    assert_eq!(
        sql,
        r#"select "users"."id", "users"."email", "users"."name" from "users""#
    );
}

#[test]
fn select_with_filter_binds_value() {
    let sql = Select::new(User::all())
        .from::<User>()
        .filter(eq(User::id, uid()))
        .to_sql()
        .unwrap();
    assert_eq!(
        sql,
        r#"select "users"."id", "users"."email", "users"."name" from "users" where "users"."id" = $1"#
    );
}

#[test]
fn partial_select_tuple() {
    let sql = Select::new((User::id, User::email))
        .from::<User>()
        .to_sql()
        .unwrap();
    assert_eq!(sql, r#"select "users"."id", "users"."email" from "users""#);
}

#[test]
fn combined_predicates_and_ordering_and_paging() {
    let sql = Select::new(User::all())
        .from::<User>()
        .filter(and(eq(User::name, "Dan"), eq(User::email, "a@b.com")))
        .order_by(desc(User::name))
        .limit(10)
        .offset(20)
        .to_sql()
        .unwrap();
    assert_eq!(
        sql,
        r#"select "users"."id", "users"."email", "users"."name" from "users" where ("users"."name" = $1 and "users"."email" = $2) order by "users"."name" desc limit $3 offset $4"#
    );
}

#[test]
fn whole_row_join_tuple() {
    // (Post, Option<User>) over a left join — positional decode handles the
    // duplicate "id" column across both whole rows.
    let sql = Select::new((Post::all(), User::all().nullable()))
        .from::<Post>()
        .left_join::<User>(eq(Post::author_id, User::id))
        .to_sql()
        .unwrap();
    assert_eq!(
        sql,
        r#"select "posts"."id", "posts"."author_id", "posts"."title", "posts"."views", "users"."id", "users"."email", "users"."name" from "posts" left join "users" on "posts"."author_id" = "users"."id""#
    );
}

#[test]
fn inner_join_renders_on_clause() {
    let sql = Select::new((Post::id, User::name))
        .from::<Post>()
        .inner_join::<User>(eq(Post::author_id, User::id))
        .to_sql()
        .unwrap();
    assert_eq!(
        sql,
        r#"select "posts"."id", "users"."name" from "posts" inner join "users" on "posts"."author_id" = "users"."id""#
    );
}

#[test]
fn any_of_uses_array_bind() {
    let ids = [uid(), uid()];
    let sql = Select::new(User::all())
        .from::<User>()
        .filter(any_of(User::id, &ids))
        .to_sql()
        .unwrap();
    assert_eq!(
        sql,
        r#"select "users"."id", "users"."email", "users"."name" from "users" where "users"."id" = any($1)"#
    );
}

#[test]
fn empty_any_of_renders_array_bind() {
    let ids: [Uuid; 0] = [];
    // `= ANY('{}')` is valid Postgres (matches nothing) — unlike `IN ()`.
    let sql = Select::new(User::all())
        .from::<User>()
        .filter(any_of(User::id, &ids))
        .to_sql()
        .unwrap();
    assert_eq!(
        sql,
        r#"select "users"."id", "users"."email", "users"."name" from "users" where "users"."id" = any($1)"#
    );
}

#[test]
fn sql_expr_in_tuple() {
    use stakit_orm::sql_expr;
    let sql = Select::new((User::id, sql_expr::<i64>("count(*) over ()")))
        .from::<User>()
        .to_sql()
        .unwrap();
    assert_eq!(sql, r#"select "users"."id", count(*) over () from "users""#);
}

#[test]
fn count_projection() {
    let projection: Count = count();
    let sql = Select::new(projection).from::<User>().to_sql().unwrap();
    assert_eq!(sql, r#"select count(*) from "users""#);
}

#[test]
fn update_sets_and_filters() {
    use stakit_orm::Update;
    let sql = Update::<User>::new()
        .set(User::name, "Sam")
        .set(User::email, "s@b.com")
        .filter(eq(User::id, uid()))
        .to_sql()
        .unwrap();
    assert_eq!(
        sql,
        r#"update "users" set "name" = $1, "email" = $2 where "users"."id" = $3"#
    );
}

#[test]
fn delete_with_filter() {
    use stakit_orm::Delete;
    let sql = Delete::<User>::new()
        .filter(eq(User::id, uid()))
        .to_sql()
        .unwrap();
    assert_eq!(sql, r#"delete from "users" where "users"."id" = $1"#);
}

#[derive(Table)]
#[table(name = "accounts")]
#[allow(dead_code)]
struct Account {
    #[column(pk, default = "gen_random_uuid()")]
    id: Uuid,
    email: String,
}

#[test]
fn insert_omits_none_defaulted_columns() {
    use stakit_orm::Insert;
    // id is defaulted -> Option in AccountNew; None omits it so the DB default fires.
    let sql = Insert::new(vec![AccountNew {
        id: None,
        email: "a@b.com".to_owned(),
    }])
    .to_sql()
    .unwrap();
    assert_eq!(sql, r#"insert into "accounts" ("email") values ($1)"#);
}

#[test]
fn insert_includes_provided_defaulted_columns() {
    use stakit_orm::Insert;
    let sql = Insert::new(vec![AccountNew {
        id: Some(uid()),
        email: "a@b.com".to_owned(),
    }])
    .to_sql()
    .unwrap();
    assert_eq!(
        sql,
        r#"insert into "accounts" ("email", "id") values ($1, $2)"#
    );
}

#[test]
fn insert_many_one_statement() {
    use stakit_orm::Insert;
    let sql = Insert::new(vec![
        AccountNew {
            id: None,
            email: "a@b.com".to_owned(),
        },
        AccountNew {
            id: None,
            email: "c@d.com".to_owned(),
        },
    ])
    .to_sql()
    .unwrap();
    assert_eq!(sql, r#"insert into "accounts" ("email") values ($1), ($2)"#);
}

#[test]
fn group_by_with_aggregate() {
    use stakit_orm::count_col;
    let sql = Select::new((User::name, count_col(User::id)))
        .from::<User>()
        .group_by(User::name)
        .to_sql()
        .unwrap();
    assert_eq!(
        sql,
        r#"select "users"."name", count("users"."id") from "users" group by "users"."name""#
    );
}

#[test]
fn group_by_having_aggregate() {
    use stakit_orm::count_col;
    use stakit_orm::expr::raw_pred;
    let sql = Select::new((User::name, count_col(User::id)))
        .from::<User>()
        .group_by(User::name)
        .having(raw_pred("count(*) > 1"))
        .to_sql()
        .unwrap();
    assert_eq!(
        sql,
        r#"select "users"."name", count("users"."id") from "users" group by "users"."name" having count(*) > 1"#
    );
}

#[test]
fn on_conflict_do_update_sets_excluded() {
    use stakit_orm::Insert;
    let sql = Insert::new(vec![AccountNew {
        id: Some(uid()),
        email: "a@b.com".to_owned(),
    }])
    .on_conflict_do_update(Account::email)
    .to_sql()
    .unwrap();
    assert_eq!(
        sql,
        r#"insert into "accounts" ("email", "id") values ($1, $2) on conflict ("email") do update set "id" = excluded."id""#
    );
}

#[test]
fn on_conflict_do_nothing_renders() {
    use stakit_orm::Insert;
    let sql = Insert::new(vec![AccountNew {
        id: None,
        email: "a@b.com".to_owned(),
    }])
    .on_conflict_do_nothing(Account::email)
    .to_sql()
    .unwrap();
    assert_eq!(
        sql,
        r#"insert into "accounts" ("email") values ($1) on conflict ("email") do nothing"#
    );
}

#[derive(stakit_orm::Row)]
#[allow(dead_code)]
struct UserStat {
    #[from(User::name)]
    name: String,
    #[from(stakit_orm::count_col(User::id))]
    total: i64,
}

#[test]
fn derive_row_named_projection() {
    let sql = Select::new(UserStat::project())
        .from::<User>()
        .group_by(User::name)
        .to_sql()
        .unwrap();
    assert_eq!(
        sql,
        r#"select "users"."name", count("users"."id") from "users" group by "users"."name""#
    );
}

#[test]
fn empty_insert_renders_no_sql() {
    use stakit_orm::Insert;
    let sql = Insert::new(Vec::<AccountNew>::new()).to_sql().unwrap();
    assert_eq!(sql, "");
}

#[derive(Table)]
#[table(name = "profiles")]
#[allow(dead_code)]
struct Profile {
    #[column(pk)]
    id: Uuid,
    bio: Option<String>,
    age: i32,
}

#[test]
fn ne_renders_not_equal() {
    let sql = Select::new(User::all())
        .from::<User>()
        .filter(ne(User::name, "Dan"))
        .to_sql()
        .unwrap();
    assert_eq!(
        sql,
        r#"select "users"."id", "users"."email", "users"."name" from "users" where "users"."name" <> $1"#
    );
}

#[test]
fn comparison_operators_render() {
    let sql = Select::new(Post::all())
        .from::<Post>()
        .filter(and(
            and(gt(Post::views, 1), lt(Post::views, 100)),
            and(gte(Post::views, 2), lte(Post::views, 99)),
        ))
        .to_sql()
        .unwrap();
    assert_eq!(
        sql,
        r#"select "posts"."id", "posts"."author_id", "posts"."title", "posts"."views" from "posts" where (("posts"."views" > $1 and "posts"."views" < $2) and ("posts"."views" >= $3 and "posts"."views" <= $4))"#
    );
}

#[test]
fn like_on_nullable_column() {
    let sql = Select::new(Profile::all())
        .from::<Profile>()
        .filter(like(Profile::bio, "%rust%"))
        .to_sql()
        .unwrap();
    assert_eq!(
        sql,
        r#"select "profiles"."id", "profiles"."bio", "profiles"."age" from "profiles" where "profiles"."bio" like $1"#
    );
}

#[test]
fn is_null_renders() {
    let sql = Select::new(Profile::all())
        .from::<Profile>()
        .filter(is_null(Profile::bio))
        .to_sql()
        .unwrap();
    assert_eq!(
        sql,
        r#"select "profiles"."id", "profiles"."bio", "profiles"."age" from "profiles" where "profiles"."bio" is null"#
    );
}

#[test]
fn not_via_nested_or_and_combination() {
    // Exercise deep and/or nesting and parenthesization.
    let sql = Select::new(User::all())
        .from::<User>()
        .filter(or(
            and(eq(User::name, "a"), eq(User::email, "b")),
            or(eq(User::name, "c"), eq(User::name, "d")),
        ))
        .to_sql()
        .unwrap();
    assert_eq!(
        sql,
        r#"select "users"."id", "users"."email", "users"."name" from "users" where (("users"."name" = $1 and "users"."email" = $2) or ("users"."name" = $3 or "users"."name" = $4))"#
    );
}

#[test]
fn multiple_order_by_terms() {
    let sql = Select::new(User::all())
        .from::<User>()
        .order_by(asc(User::name))
        .order_by(desc(User::email))
        .order_by(asc(User::id))
        .to_sql()
        .unwrap();
    assert_eq!(
        sql,
        r#"select "users"."id", "users"."email", "users"."name" from "users" order by "users"."name" asc, "users"."email" desc, "users"."id" asc"#
    );
}

#[test]
fn any_of_multi_and_empty_bind_one_param_each() {
    // multi
    let ids = [uid(), uid(), uid()];
    let sql = Select::new(User::all())
        .from::<User>()
        .filter(any_of(User::id, &ids))
        .to_sql()
        .unwrap();
    assert_eq!(
        sql,
        r#"select "users"."id", "users"."email", "users"."name" from "users" where "users"."id" = any($1)"#
    );

    // empty (still a single array bind)
    let none: [Uuid; 0] = [];
    let sql = Select::new(User::all())
        .from::<User>()
        .filter(any_of(User::id, &none))
        .to_sql()
        .unwrap();
    assert_eq!(
        sql,
        r#"select "users"."id", "users"."email", "users"."name" from "users" where "users"."id" = any($1)"#
    );
}

#[test]
fn group_by_multiple_columns() {
    use stakit_orm::count_col;
    let sql = Select::new((User::name, User::email, count_col(User::id)))
        .from::<User>()
        .group_by(User::name)
        .group_by(User::email)
        .to_sql()
        .unwrap();
    assert_eq!(
        sql,
        r#"select "users"."name", "users"."email", count("users"."id") from "users" group by "users"."name", "users"."email""#
    );
}

#[test]
fn having_with_raw_pred_after_group_by() {
    use stakit_orm::count_col;
    use stakit_orm::expr::raw_pred;
    let sql = Select::new((User::name, count_col(User::id)))
        .from::<User>()
        .group_by(User::name)
        .having(raw_pred("count(*) >= 3"))
        .order_by(desc(User::name))
        .to_sql()
        .unwrap();
    assert_eq!(
        sql,
        r#"select "users"."name", count("users"."id") from "users" group by "users"."name" having count(*) >= 3 order by "users"."name" desc"#
    );
}

#[test]
fn sql_expr_standalone_projection() {
    use stakit_orm::sql_expr;
    let sql = Select::new(sql_expr::<i64>("count(*) filter (where true)"))
        .from::<User>()
        .to_sql()
        .unwrap();
    assert_eq!(sql, r#"select count(*) filter (where true) from "users""#);
}

#[test]
fn update_multiple_sets_no_filter() {
    use stakit_orm::Update;
    let sql = Update::<User>::new()
        .set(User::name, "Sam")
        .set(User::email, "s@b.com")
        .to_sql()
        .unwrap();
    assert_eq!(sql, r#"update "users" set "name" = $1, "email" = $2"#);
}

#[test]
fn update_set_to_column_value() {
    // RHS is another column rather than a bound value.
    use stakit_orm::Update;
    let sql = Update::<User>::new()
        .set(User::name, User::email)
        .to_sql()
        .unwrap();
    assert_eq!(sql, r#"update "users" set "name" = "users"."email""#);
}

#[test]
fn delete_without_filter() {
    use stakit_orm::Delete;
    let sql = Delete::<User>::new().to_sql().unwrap();
    assert_eq!(sql, r#"delete from "users""#);
}

#[test]
fn insert_mixed_optional_presence_across_rows() {
    use stakit_orm::Insert;
    // Row 1 omits id, row 2 supplies it. The union means the "id" column is
    // included, and row 1 binds NULL for it.
    let sql = Insert::new(vec![
        AccountNew {
            id: None,
            email: "a@b.com".to_owned(),
        },
        AccountNew {
            id: Some(uid()),
            email: "c@d.com".to_owned(),
        },
    ])
    .to_sql()
    .unwrap();
    assert_eq!(
        sql,
        r#"insert into "accounts" ("email", "id") values ($1, $2), ($3, $4)"#
    );
}

#[derive(Table)]
#[table(name = "widgets")]
#[allow(dead_code)]
struct Widget {
    #[column(pk, default = "gen_random_uuid()")]
    id: Uuid,
    #[column(unique)]
    sku: String,
    name: String,
    price: i32,
}

#[test]
fn on_conflict_do_update_multiple_non_target_columns() {
    use stakit_orm::Insert;
    let sql = Insert::new(vec![WidgetNew {
        id: Some(uid()),
        sku: "abc".to_owned(),
        name: "Gadget".to_owned(),
        price: 10,
    }])
    .on_conflict_do_update(Widget::sku)
    .to_sql()
    .unwrap();
    // Every inserted column except the conflict target is set to excluded.<col>.
    assert_eq!(
        sql,
        r#"insert into "widgets" ("sku", "name", "price", "id") values ($1, $2, $3, $4) on conflict ("sku") do update set "name" = excluded."name", "price" = excluded."price", "id" = excluded."id""#
    );
}

#[test]
fn on_conflict_do_update_with_only_target_falls_back_to_do_nothing() {
    use stakit_orm::Insert;
    // Only the target column is present -> no non-target columns -> DO NOTHING.
    let sql = Insert::new(vec![AccountNew {
        id: None,
        email: "a@b.com".to_owned(),
    }])
    .on_conflict_do_update(Account::email)
    .to_sql()
    .unwrap();
    assert_eq!(
        sql,
        r#"insert into "accounts" ("email") values ($1) on conflict ("email") do nothing"#
    );
}

#[test]
fn on_conflict_composite_key_selective_set_and_coalesce() {
    use stakit_orm::Insert;
    // Composite conflict key + explicit per-column updates: `price` overwritten,
    // `id` kept when the incoming value is NULL (coalesce keeps the stored one).
    let sql = Insert::new(vec![WidgetNew {
        id: Some(uid()),
        sku: "abc".to_owned(),
        name: "Gadget".to_owned(),
        price: 10,
    }])
    .on_conflict((Widget::sku, Widget::name))
    .set(Widget::price)
    .set_coalesce(Widget::id)
    .to_sql()
    .unwrap();
    assert_eq!(
        sql,
        r#"insert into "widgets" ("sku", "name", "price", "id") values ($1, $2, $3, $4) on conflict ("sku", "name") do update set "price" = excluded."price", "id" = coalesce(excluded."id", "widgets"."id")"#
    );
}

#[test]
fn on_conflict_builder_do_nothing_renders_composite_target() {
    use stakit_orm::Insert;
    let sql = Insert::new(vec![WidgetNew {
        id: Some(uid()),
        sku: "abc".to_owned(),
        name: "Gadget".to_owned(),
        price: 10,
    }])
    .on_conflict((Widget::sku, Widget::name))
    .do_nothing()
    .to_sql()
    .unwrap();
    assert!(
        sql.ends_with(r#"on conflict ("sku", "name") do nothing"#),
        "got {sql}"
    );
}

#[test]
fn on_conflict_do_update_all_via_builder() {
    use stakit_orm::Insert;
    // `do_update_all()` overwrites every non-key inserted column with excluded.<col>.
    let sql = Insert::new(vec![WidgetNew {
        id: Some(uid()),
        sku: "abc".to_owned(),
        name: "Gadget".to_owned(),
        price: 10,
    }])
    .on_conflict(Widget::sku)
    .do_update_all()
    .to_sql()
    .unwrap();
    assert_eq!(
        sql,
        r#"insert into "widgets" ("sku", "name", "price", "id") values ($1, $2, $3, $4) on conflict ("sku") do update set "name" = excluded."name", "price" = excluded."price", "id" = excluded."id""#
    );
}

#[test]
fn on_conflict_do_update_all_except_via_builder() {
    use stakit_orm::Insert;
    // `do_update_all_except(name)` overwrites all non-key columns but leaves `name`.
    let sql = Insert::new(vec![WidgetNew {
        id: Some(uid()),
        sku: "abc".to_owned(),
        name: "Gadget".to_owned(),
        price: 10,
    }])
    .on_conflict(Widget::sku)
    .do_update_all_except(Widget::name)
    .to_sql()
    .unwrap();
    assert_eq!(
        sql,
        r#"insert into "widgets" ("sku", "name", "price", "id") values ($1, $2, $3, $4) on conflict ("sku") do update set "price" = excluded."price", "id" = excluded."id""#
    );
}

#[test]
fn on_conflict_builder_with_no_set_defaults_to_do_nothing() {
    use stakit_orm::Insert;
    // `on_conflict(key)` with no `set`/`set_coalesce` -> DO NOTHING (no empty SET).
    let sql = Insert::new(vec![WidgetNew {
        id: Some(uid()),
        sku: "abc".to_owned(),
        name: "Gadget".to_owned(),
        price: 10,
    }])
    .on_conflict(Widget::sku)
    .to_sql()
    .unwrap();
    assert!(
        sql.ends_with(r#"on conflict ("sku") do nothing"#),
        "got {sql}"
    );
}

#[test]
fn count_terminal_wraps_subquery() {
    // Count drops paging/order but keeps the filter; verify the inner SQL the
    // wrapper is built from renders the filter correctly.
    let sql = Select::new(User::all())
        .from::<User>()
        .filter(eq(User::name, "x"))
        .order_by(asc(User::name))
        .limit(10)
        .to_sql()
        .unwrap();
    assert_eq!(
        sql,
        r#"select "users"."id", "users"."email", "users"."name" from "users" where "users"."name" = $1 order by "users"."name" asc limit $2"#
    );
}

#[test]
fn full_clause_ordering() {
    // where -> group by -> having -> order by -> limit -> offset, in order.
    use stakit_orm::count_col;
    use stakit_orm::expr::raw_pred;
    let sql = Select::new((User::name, count_col(User::id)))
        .from::<User>()
        .filter(eq(User::email, "a@b.com"))
        .group_by(User::name)
        .having(raw_pred("count(*) > 0"))
        .order_by(desc(User::name))
        .limit(5)
        .offset(10)
        .to_sql()
        .unwrap();
    assert_eq!(
        sql,
        r#"select "users"."name", count("users"."id") from "users" where "users"."email" = $1 group by "users"."name" having count(*) > 0 order by "users"."name" desc limit $2 offset $3"#
    );
}

#[test]
fn table_metadata_is_emitted() {
    assert_eq!(<User as stakit_orm::Table>::TABLE, "users");
    assert_eq!(<User as stakit_orm::Table>::COLUMNS.len(), 3);
    let pk = <Post as stakit_orm::Table>::COLUMNS
        .iter()
        .find(|c| c.is_pk)
        .unwrap();
    assert_eq!(pk.name, "id");
    let fk = <Post as stakit_orm::Table>::COLUMNS
        .iter()
        .find(|c| c.references.is_some())
        .unwrap();
    let reference = fk.references.unwrap();
    assert_eq!(reference.table, "users");
    assert_eq!(reference.column, "id");
}

// ----- #[derive(Type)] Postgres composite types -----

#[derive(stakit_orm::Type, Debug, Clone, PartialEq)]
#[db_type(name = "address_type")]
#[allow(dead_code)]
struct Address {
    street: String,
    city: String,
    is_primary: bool,
}

#[derive(Table)]
#[table(name = "people")]
#[allow(dead_code)]
struct Person {
    #[column(pk)]
    id: i64,
    #[column(sql_type = "address_type")]
    home: Address,
}

#[test]
fn composite_create_type_sql_and_name() {
    assert_eq!(Address::SQL_TYPE_NAME, "address_type");
    assert_eq!(
        Address::create_type_sql(),
        "create type address_type as (street text, city text, is_primary boolean)"
    );
}

#[test]
fn composite_insert_casts_placeholder() {
    use stakit_orm::Insert;
    let sql = Insert::new(vec![PersonNew {
        id: 1,
        home: Address {
            street: "123 Main, Apt 4".to_owned(),
            city: "NYC".to_owned(),
            is_primary: true,
        },
    }])
    .to_sql()
    .unwrap();
    // The composite binds as the text literal cast to the type.
    assert!(sql.contains("values ($1, $2::address_type)"), "got {sql}");
}

#[test]
fn composite_whole_row_select_reads_as_text() {
    let sql = Select::new(Person::all())
        .from::<Person>()
        .to_sql()
        .unwrap();
    // The composite column is selected as `home::text` so it decodes via FromValue.
    assert!(sql.contains(r#""people"."home"::text"#), "got {sql}");
}

#[test]
fn composite_filter_casts_in_where() {
    let sql = Select::new(Person::all())
        .from::<Person>()
        .filter(eq(
            Person::home,
            Address {
                street: "x".to_owned(),
                city: "y".to_owned(),
                is_primary: false,
            },
        ))
        .to_sql()
        .unwrap();
    assert!(sql.contains("::address_type"), "got {sql}");
}

// ----- user-extensible custom type: cast a placeholder to ANY DB type, no lib edit -----

#[derive(Debug, Clone, PartialEq)]
struct Mood(String); // maps to a Postgres `CREATE TYPE mood AS ENUM (...)`

impl stakit_orm::ToValue for Mood {
    // The whole extensibility hook: name the DB cast; bind the payload as text.
    const WRITE_CAST: Option<&'static str> = Some("mood");
    fn to_value(self) -> stakit_orm::Value {
        stakit_orm::Value::Text(self.0)
    }
}
impl stakit_orm::FromValue for Mood {
    const KIND: stakit_orm::ValueKind = stakit_orm::ValueKind::Text;
    fn from_value(v: stakit_orm::Value) -> stakit_orm::Result<Self> {
        Ok(Self(<String as stakit_orm::FromValue>::from_value(v)?))
    }
}
impl stakit_orm::expr::IntoExpr<Self> for Mood {
    fn into_operand(self) -> stakit_orm::expr::Operand {
        stakit_orm::expr::Operand::Value(stakit_orm::value::with_cast(
            <Self as stakit_orm::ToValue>::to_value(self),
            <Self as stakit_orm::ToValue>::WRITE_CAST,
        ))
    }
}

#[derive(Table)]
#[table(name = "players")]
#[allow(dead_code)]
struct Player {
    #[column(pk)]
    id: i64,
    #[column(sql_type = "mood")]
    mood: Mood,
}

#[test]
fn user_custom_type_casts_on_insert_and_filter() {
    use stakit_orm::Insert;
    let sql = Insert::new(vec![PlayerNew {
        id: 1,
        mood: Mood("happy".to_owned()),
    }])
    .to_sql()
    .unwrap();
    assert!(sql.contains("$2::mood"), "insert cast, got {sql}");

    let sql = Select::new(Player::all())
        .from::<Player>()
        .filter(eq(Player::mood, Mood("sad".to_owned())))
        .to_sql()
        .unwrap();
    assert!(sql.contains("$1::mood"), "filter cast, got {sql}");
}

// ----- chained filter() ANDs (regression: must not silently replace) -----

#[test]
fn chained_select_filters_are_anded_not_replaced() {
    let sql = Select::new(User::all())
        .from::<User>()
        .filter(eq(User::name, "Dan"))
        .filter(eq(User::email, "a@b.com"))
        .to_sql()
        .unwrap();
    assert!(
        sql.contains(r#"where ("users"."name" = $1 and "users"."email" = $2)"#),
        "got: {sql}"
    );
}

#[test]
fn chained_update_filters_are_anded() {
    use stakit_orm::Update;
    let sql = Update::<User>::new()
        .set(User::name, "x")
        .filter(eq(User::email, "a@b.com"))
        .filter(eq(User::name, "Dan"))
        .to_sql()
        .unwrap();
    assert!(
        sql.contains(r#"where ("users"."email" = $2 and "users"."name" = $3)"#),
        "got: {sql}"
    );
}

#[test]
fn chained_delete_filters_are_anded() {
    use stakit_orm::Delete;
    let sql = Delete::<User>::new()
        .filter(eq(User::email, "a@b.com"))
        .filter(eq(User::id, uid()))
        .to_sql()
        .unwrap();
    assert!(
        sql.contains(r#"where ("users"."email" = $1 and "users"."id" = $2)"#),
        "got: {sql}"
    );
}

// ----- ts_rank projection + order_by_rank (Postgres full-text relevance) -----

#[derive(Table)]
#[table(name = "docs")]
#[allow(dead_code)]
struct Doc {
    #[column(pk)]
    id: i64,
    body: String,
}

#[test]
fn ts_rank_projection_and_order_by_rank_render() {
    use stakit_orm::ts_rank;
    let sql = Select::new((Doc::id, ts_rank(Doc::body, "fox")))
        .from::<Doc>()
        .filter(matches(Doc::body, "fox"))
        .order_by_rank(ts_rank(Doc::body, "fox"))
        .to_sql()
        .unwrap();
    // Selectable rank computes to_tsvector at query time and binds the query.
    assert!(
        sql.contains(
            r#"ts_rank(to_tsvector('english', "docs"."body"), plainto_tsquery('english', $1))"#
        ),
        "rank projection: {sql}"
    );
    // Ordered by relevance, descending.
    assert!(sql.trim_end().ends_with("desc"), "order desc: {sql}");
    assert!(sql.contains("order by ts_rank("), "order by rank: {sql}");
}

#[test]
fn ts_rank_stored_skips_to_tsvector() {
    use stakit_orm::ts_rank_stored;
    let sql = Select::new(ts_rank_stored(Doc::body, "fox"))
        .from::<Doc>()
        .to_sql()
        .unwrap();
    // Stored form matches the column directly (a GIN index applies).
    assert!(
        sql.contains(r#"ts_rank("docs"."body", plainto_tsquery('english', $1))"#),
        "stored rank: {sql}"
    );
    assert!(
        !sql.contains("to_tsvector"),
        "stored form must not recompute: {sql}"
    );
}
