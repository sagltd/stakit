//! `up`/`down`/`status` subcommands — apply, revert, and report sqlx migrations
//! against `$DATABASE_URL`.

use sqlx::PgPool;
use sqlx::Row;
use sqlx::migrate::Migrator;
use std::path::Path;

/// Run a migration action (`up` / `down` / `status`) against `url`.
pub async fn run(action: &str, dir: &Path, url: &str) -> Result<String, String> {
    let migrator = Migrator::new(dir)
        .await
        .map_err(|error| error.to_string())?;
    let pool = PgPool::connect(url)
        .await
        .map_err(|error| error.to_string())?;
    match action {
        "up" => apply(&migrator, &pool).await,
        "down" => revert(&migrator, &pool).await,
        "status" => status(&migrator, &pool).await,
        other => Err(format!("unknown migration action: {other}")),
    }
}

async fn apply(migrator: &Migrator, pool: &PgPool) -> Result<String, String> {
    migrator
        .run(pool)
        .await
        .map_err(|error| error.to_string())?;
    Ok("migrations applied".to_owned())
}

/// Revert the most recently applied migration, then report what actually
/// changed (a non-reversible latest migration is a no-op, reported as such).
async fn revert(migrator: &Migrator, pool: &PgPool) -> Result<String, String> {
    let before = applied_versions(pool).await?;
    if before.is_empty() {
        return Ok("nothing to revert".to_owned());
    }
    // Keep everything up to the second-latest; i64::MIN reverts the only one.
    let target = before.get(1).copied().unwrap_or(i64::MIN);
    migrator
        .undo(pool, target)
        .await
        .map_err(|error| error.to_string())?;
    let after = applied_versions(pool).await?;
    let reverted: Vec<i64> = before
        .iter()
        .copied()
        .filter(|v| !after.contains(v))
        .collect();
    if reverted.is_empty() {
        Ok("nothing reverted (latest migration has no .down.sql)".to_owned())
    } else {
        Ok(format!(
            "reverted {} migration(s): {reverted:?}",
            reverted.len()
        ))
    }
}

async fn status(migrator: &Migrator, pool: &PgPool) -> Result<String, String> {
    let applied = applied_versions(pool).await?;
    // Reversible migrations load as up+down pairs; count only forward ones so
    // `total` matches the one-row-per-version `_sqlx_migrations` accounting.
    let total = migrator
        .iter()
        .filter(|m| !m.migration_type.is_down_migration())
        .count();
    let pending = total.saturating_sub(applied.len());
    Ok(format!(
        "{} applied, {pending} pending, {total} total",
        applied.len()
    ))
}

/// Applied migration versions, newest first. Empty only if the tracking table
/// does not exist yet (SQLSTATE 42P01); other errors propagate.
async fn applied_versions(pool: &PgPool) -> Result<Vec<i64>, String> {
    let query = "select version from _sqlx_migrations order by version desc";
    match sqlx::query(query).fetch_all(pool).await {
        Ok(rows) => rows
            .iter()
            .map(|row| {
                row.try_get::<i64, _>("version")
                    .map_err(|error| error.to_string())
            })
            .collect(),
        Err(error) => {
            let undefined_table = error
                .as_database_error()
                .and_then(sqlx::error::DatabaseError::code)
                .is_some_and(|code| code == "42P01");
            if undefined_table {
                Ok(Vec::new())
            } else {
                Err(error.to_string())
            }
        }
    }
}
