//! Boot an embedded Postgres (`postgresql_embedded`, no Docker) for the
//! Postgres-backed tests and benches. Included with `#[path]`.
//!
//! `postgresql_embedded` downloads a real Postgres binary on first run (cached
//! afterward), so the first boot needs network access.
#![allow(dead_code, unreachable_pub)]

/// Start a fresh server with database `name`, returning the server (keep it
/// alive for the test's duration) and the database URL.
pub async fn boot(name: &str) -> (postgresql_embedded::PostgreSQL, String) {
    let mut postgres = postgresql_embedded::PostgreSQL::default();
    postgres.setup().await.expect("setup embedded postgres");
    postgres.start().await.expect("start embedded postgres");
    postgres
        .create_database(name)
        .await
        .expect("create database");
    let url = postgres.settings().url(name);
    (postgres, url)
}

/// Apply `statements` in order, panicking with the failing statement.
pub async fn run(pool: &sqlx::PgPool, statements: &'static [&'static str]) {
    for statement in statements {
        sqlx::query(*statement)
            .execute(pool)
            .await
            .unwrap_or_else(|error| panic!("apply `{statement}`: {error}"));
    }
}
