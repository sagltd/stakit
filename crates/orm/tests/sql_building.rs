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
