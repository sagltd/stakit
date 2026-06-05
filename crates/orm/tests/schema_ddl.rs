#![allow(missing_docs)]
//! Schema-metadata tests for `#[derive(Table)]`: that index access methods,
//! operator classes, and composite primary keys flow into `COLUMNS` and render the
//! right `CREATE INDEX` DDL. Pure (no database) — runs on every backend build.

use stakit_orm::Table as _;

#[derive(stakit_orm::Table, Debug)]
#[table(name = "docs")]
#[allow(dead_code)]
struct Doc {
    #[column(pk)]
    id: i64,
    body: String,
    #[column(
        sql_type = "vector(3)",
        index,
        index_method = "hnsw",
        opclass = "vector_cosine_ops"
    )]
    embedding: String,
    #[column(sql_type = "jsonb", index, index_method = "gin")]
    tags: serde_json::Value,
    #[column(index)]
    author: String,
    // Stored generated column: excluded from COLUMNS, still gets a `Col` token.
    #[column(
        sql_type = "tsvector",
        generated = "to_tsvector('english', body)",
        index = "gin"
    )]
    body_tsv: String,
}

fn column(name: &str) -> &'static stakit_orm::Column {
    Doc::COLUMNS
        .iter()
        .find(|c| c.name == name)
        .expect("column present")
}

#[test]
fn hnsw_column_carries_method_and_opclass() {
    let embedding = column("embedding");
    assert!(embedding.is_index);
    assert_eq!(embedding.index_method, Some("hnsw"));
    assert_eq!(embedding.index_opclass, Some("vector_cosine_ops"));
}

#[test]
fn hnsw_column_renders_using_method_with_opclass() {
    assert_eq!(
        column("embedding").create_index_sql("docs").as_deref(),
        Some(
            r#"create index "idx_docs_embedding" on "docs" using hnsw ("embedding" vector_cosine_ops)"#
        ),
    );
}

#[test]
fn gin_column_renders_method_without_opclass() {
    assert_eq!(
        column("tags").create_index_sql("docs").as_deref(),
        Some(r#"create index "idx_docs_tags" on "docs" using gin ("tags")"#),
    );
}

#[test]
fn bare_index_renders_btree_without_using() {
    assert_eq!(
        column("author").create_index_sql("docs").as_deref(),
        Some(r#"create index "idx_docs_author" on "docs" ("author")"#),
    );
}

#[test]
fn generated_column_is_excluded_from_columns() {
    // A generated column is database-computed: it must not be SELECTed/decoded as a
    // whole-row cell (a `tsvector` has no scalar decode), so it is absent from COLUMNS.
    assert!(
        !Doc::COLUMNS.iter().any(|c| c.name == "body_tsv"),
        "generated column must be excluded from COLUMNS"
    );
}

#[test]
fn generated_column_still_has_a_col_token() {
    // …but it keeps its `Col` token so it can be referenced in predicates (matches_tsv).
    assert_eq!(Doc::body_tsv.name, "body_tsv");
}

// ----- composite primary keys -----

/// A junction table with a two-column primary key (the previously-unbuildable shape).
#[derive(stakit_orm::Table, Debug)]
#[table(name = "goal_step")]
#[allow(dead_code)]
struct GoalStep {
    #[column(pk)]
    goal_id: uuid::Uuid,
    #[column(pk)]
    step_id: uuid::Uuid,
    position: i32,
}

#[test]
fn composite_key_table_marks_every_key_column() {
    let pk_columns: Vec<&str> = GoalStep::COLUMNS
        .iter()
        .filter(|c| c.is_pk)
        .map(|c| c.name)
        .collect();
    assert_eq!(pk_columns, vec!["goal_id", "step_id"]);
}

#[test]
fn composite_key_pk_type_is_a_tuple() {
    // `Pk` is `(Uuid, Uuid)`; this only compiles if the derive emitted the tuple.
    fn assert_pk_is<T: stakit_orm::Table<Pk = P>, P>() {}
    assert_pk_is::<GoalStep, (uuid::Uuid, uuid::Uuid)>();
}

/// A single-key table still exposes its scalar key type (no regression).
#[derive(stakit_orm::Table, Debug)]
#[table(name = "single_key")]
#[allow(dead_code)]
struct SingleKey {
    #[column(pk)]
    id: i64,
    label: String,
}

#[test]
fn single_key_pk_type_is_the_scalar() {
    fn assert_pk_is<T: stakit_orm::Table<Pk = P>, P>() {}
    assert_pk_is::<SingleKey, i64>();
}

// ---- Row-level security: the derives compile through the public API ----

#[derive(stakit_orm::Role)]
#[role(name = "app_user", login)]
struct AppUser;

#[derive(stakit_orm::Table, Debug)]
#[table(
    name = "rls_posts",
    rls,
    force_rls,
    grant(app_user(select, insert, update, delete)),
    policy(
        rls_posts_owner(
            select,
            to = "app_user",
            using = "author_id = current_setting('app.user_id')::uuid"
        ),
        rls_posts_insert(
            insert,
            to = "app_user",
            check = "author_id = current_setting('app.user_id')::uuid"
        )
    )
)]
#[allow(dead_code)]
struct RlsPost {
    #[column(pk)]
    id: i64,
    author_id: String,
    title: String,
}

#[test]
fn role_derive_exposes_role_name() {
    assert_eq!(AppUser::ROLE, "app_user");
}

#[test]
fn rls_table_columns_are_unaffected_by_policies_and_grants() {
    // RLS attributes are migration-only: they must not perturb the runtime column set.
    let names: Vec<&str> = RlsPost::COLUMNS.iter().map(|c| c.name).collect();
    assert_eq!(names, vec!["id", "author_id", "title"]);
}
