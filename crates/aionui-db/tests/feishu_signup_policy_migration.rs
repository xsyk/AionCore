//! Migration 047 on a database that already has a Feishu login config row.

use std::borrow::Cow;
use std::path::Path;

use sqlx::Row;
use sqlx::migrate::Migrator;
use sqlx::sqlite::SqlitePoolOptions;

async fn run_migrations_through(pool: &sqlx::SqlitePool, max_version: i64) {
    let full = Migrator::new(Path::new("migrations")).await.unwrap();
    let migrations = full
        .migrations
        .iter()
        .filter(|migration| migration.version <= max_version)
        .cloned()
        .collect::<Vec<_>>();
    let migrator = Migrator {
        migrations: Cow::Owned(migrations),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    migrator.run(pool).await.unwrap();
}

async fn run_migration(pool: &sqlx::SqlitePool, version: i64) {
    let full = Migrator::new(Path::new("migrations")).await.unwrap();
    let migrations = full
        .migrations
        .iter()
        .filter(|migration| migration.version == version)
        .cloned()
        .collect::<Vec<_>>();
    let migrator = Migrator {
        migrations: Cow::Owned(migrations),
        ignore_missing: true,
        locking: true,
        no_tx: false,
    };
    migrator.run(pool).await.unwrap();
}

async fn memory_pool() -> sqlx::SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    // The connection PRAGMAs the production migrator sets (database.rs);
    // migration 030 rebuilds tables and needs them.
    sqlx::query("PRAGMA foreign_keys = OFF; PRAGMA legacy_alter_table = ON")
        .execute(&pool)
        .await
        .unwrap();
    pool
}

#[tokio::test]
async fn existing_config_row_gets_approval_policy() {
    let pool = memory_pool().await;
    run_migrations_through(&pool, 46).await;
    sqlx::query(
        "INSERT INTO feishu_login_config (id, enabled, app_id, public_base_url, updated_at) \
         VALUES (1, 1, 'cli_old', 'https://aidi.example.com', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();

    run_migration(&pool, 47).await;

    let row = sqlx::query("SELECT app_id, enabled, signup_policy FROM feishu_login_config WHERE id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("app_id"), "cli_old");
    assert!(row.get::<bool, _>("enabled"));
    assert_eq!(row.get::<String, _>("signup_policy"), "approval");
    let bad = sqlx::query("UPDATE feishu_login_config SET signup_policy = 'anyone' WHERE id = 1")
        .execute(&pool)
        .await;
    assert!(bad.is_err(), "CHECK constraint applies to upgraded rows");
}
