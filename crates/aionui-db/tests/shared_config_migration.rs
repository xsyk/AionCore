//! Migration 048: custom agents become server-wide, and the `global_settings`
//! table is created with the super admin's image generation choice carried over.
//!
//! Each test brings a database to the 047 schema, seeds what an upgraded server
//! really holds, runs only migration 048 and reads the result back.

use std::borrow::Cow;
use std::path::Path;

use serde_json::{Value, json};
use sqlx::Row;
use sqlx::migrate::Migrator;
use sqlx::sqlite::SqlitePoolOptions;

const ADMIN: &str = "system_default_user";
const ALICE: &str = "user_alice";
const IMAGE_PREFERENCE: &str = "tools.imageGenerationModel";
const GLOBAL_IMAGE_SETTING: &str = "tools.imageGeneration";

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
    assert_eq!(migrations.len(), 1, "migration {version} must exist");
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

/// A server on the 047 schema with the super admin and one ordinary user.
async fn pool_at_047() -> sqlx::SqlitePool {
    let pool = memory_pool().await;
    run_migrations_through(&pool, 47).await;
    for (id, username) in [(ADMIN, "admin"), (ALICE, "alice")] {
        sqlx::query(
            "INSERT INTO users (id, user_type, username, password_hash, status, session_generation, created_at, updated_at) \
             VALUES (?, 'local', ?, 'hash', 'active', 0, 1, 1)",
        )
        .bind(id)
        .bind(username)
        .execute(&pool)
        .await
        .unwrap();
    }
    pool
}

/// Insert an agent row the way the app stores it. `owner` is the creator's id
/// for a user-defined agent, `None` for a catalog row.
async fn insert_agent(pool: &sqlx::SqlitePool, agent_id: &str, source: &str, owner: Option<&str>) {
    sqlx::query(
        "INSERT INTO agent_metadata \
            (id, agent_id, user_id, name, agent_type, agent_source, enabled, command, args, env, sort_order, created_at, updated_at) \
         VALUES (?, ?, ?, ?, 'acp', ?, 1, 'my-agent', '[\"--acp\"]', '[{\"name\":\"K\",\"value\":\"v\"}]', 1500, 1, 1)",
    )
    .bind(format!("row-{agent_id}"))
    .bind(agent_id)
    .bind(owner)
    .bind(format!("Agent {agent_id}"))
    .bind(source)
    .execute(pool)
    .await
    .unwrap();
}

/// `(agent_id, agent_source, user_id)` of every row, ordered, so two snapshots compare.
async fn ownership(pool: &sqlx::SqlitePool) -> Vec<(String, String, Option<String>)> {
    sqlx::query("SELECT agent_id, agent_source, user_id FROM agent_metadata ORDER BY agent_id")
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|row| (row.get("agent_id"), row.get("agent_source"), row.get("user_id")))
        .collect()
}

async fn set_preference(pool: &sqlx::SqlitePool, user_id: &str, key: &str, value: &str, updated_at: i64) {
    sqlx::query("INSERT INTO client_preferences (user_id, key, value, updated_at) VALUES (?, ?, ?, ?)")
        .bind(user_id)
        .bind(key)
        .bind(value)
        .bind(updated_at)
        .execute(pool)
        .await
        .unwrap();
}

/// Every `global_settings` row as `(key, value, updated_at)`, the value parsed.
async fn global_settings(pool: &sqlx::SqlitePool) -> Vec<(String, Value, i64)> {
    sqlx::query("SELECT key, value, updated_at FROM global_settings ORDER BY key")
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|row| {
            let raw: String = row.get("value");
            (
                row.get("key"),
                serde_json::from_str(&raw).expect("global_settings.value is JSON"),
                row.get("updated_at"),
            )
        })
        .collect()
}

#[tokio::test]
async fn every_custom_agent_becomes_server_wide() {
    let pool = pool_at_047().await;
    insert_agent(&pool, "custom-of-admin", "custom", Some(ADMIN)).await;
    insert_agent(&pool, "custom-of-alice", "custom", Some(ALICE)).await;
    insert_agent(&pool, "custom-already-global", "custom", None).await;
    let before = ownership(&pool).await;

    run_migration(&pool, 48).await;

    let after = ownership(&pool).await;
    assert_eq!(after.len(), before.len(), "no row is added or removed");
    for (agent_id, source, owner) in &after {
        if source == "custom" {
            assert_eq!(owner, &None, "custom agent {agent_id} must have no owner");
        }
    }
    // Only ownership changed: the agent itself is exactly as it was saved.
    let row =
        sqlx::query("SELECT name, command, args, env, enabled FROM agent_metadata WHERE agent_id = 'custom-of-alice'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row.get::<String, _>("name"), "Agent custom-of-alice");
    assert_eq!(row.get::<String, _>("command"), "my-agent");
    assert_eq!(row.get::<String, _>("args"), r#"["--acp"]"#);
    assert_eq!(row.get::<String, _>("env"), r#"[{"name":"K","value":"v"}]"#);
    assert!(row.get::<bool, _>("enabled"));
}

#[tokio::test]
async fn catalog_rows_are_left_as_they_were() {
    let pool = pool_at_047().await;
    insert_agent(&pool, "custom-of-alice", "custom", Some(ALICE)).await;
    let before: Vec<_> = ownership(&pool)
        .await
        .into_iter()
        .filter(|(_, source, _)| source != "custom")
        .collect();
    assert!(
        before.iter().any(|(_, source, _)| source == "builtin")
            && before.iter().any(|(_, source, _)| source == "internal"),
        "the seeded catalog has builtin and internal rows"
    );

    run_migration(&pool, 48).await;

    let after: Vec<_> = ownership(&pool)
        .await
        .into_iter()
        .filter(|(_, source, _)| source != "custom")
        .collect();
    assert_eq!(after, before, "builtin and internal rows keep their owner (none)");
}

#[tokio::test]
async fn the_admins_image_generation_choice_becomes_a_global_setting() {
    let pool = pool_at_047().await;
    // The shape the settings page saves.
    set_preference(
        &pool,
        ADMIN,
        IMAGE_PREFERENCE,
        &json!({
            "id": "p1", "name": "Gemini", "platform": "gemini", "base_url": "", "api_key": "",
            "use_model": "m1", "switch": true
        })
        .to_string(),
        1234,
    )
    .await;
    // Another user's own choice is personal and is not what the server now uses.
    set_preference(
        &pool,
        ALICE,
        IMAGE_PREFERENCE,
        &json!({"id": "p2", "use_model": "m2", "switch": true}).to_string(),
        5678,
    )
    .await;

    run_migration(&pool, 48).await;

    assert_eq!(
        global_settings(&pool).await,
        vec![(
            GLOBAL_IMAGE_SETTING.to_owned(),
            json!({"provider_id": "p1", "model": "m1", "enabled": true}),
            1234
        )]
    );
    // Nothing is taken away from the personal preferences.
    let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM client_preferences WHERE key = ?")
        .bind(IMAGE_PREFERENCE)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(kept, 2);
}

#[tokio::test]
async fn a_choice_that_was_switched_off_stays_off() {
    for value in [
        json!({"id": "p1", "use_model": "m1", "switch": false}),
        json!({"id": "p1", "use_model": "m1"}),
    ] {
        let pool = pool_at_047().await;
        set_preference(&pool, ADMIN, IMAGE_PREFERENCE, &value.to_string(), 99).await;

        run_migration(&pool, 48).await;

        assert_eq!(
            global_settings(&pool).await,
            vec![(
                GLOBAL_IMAGE_SETTING.to_owned(),
                json!({"provider_id": "p1", "model": "m1", "enabled": false}),
                99
            )],
            "saved as {value}"
        );
    }
}

#[tokio::test]
async fn no_setting_is_made_up_when_the_admin_never_chose_a_model() {
    let unusable = [
        // Only an ordinary user chose a model.
        None,
        // The admin's value is not JSON.
        Some("not json at all".to_owned()),
        Some(String::new()),
        // The admin's value names no provider or no model.
        Some(json!({"use_model": "m1", "switch": true}).to_string()),
        Some(json!({"id": "p1", "switch": true}).to_string()),
        Some(json!({"id": null, "use_model": "m1", "switch": true}).to_string()),
        // The admin's value is JSON but not an object.
        Some(json!(["p1", "m1"]).to_string()),
        Some(json!("p1").to_string()),
        Some(json!(null).to_string()),
    ];
    for admin_value in unusable {
        let pool = pool_at_047().await;
        set_preference(
            &pool,
            ALICE,
            IMAGE_PREFERENCE,
            &json!({"id": "p2", "use_model": "m2", "switch": true}).to_string(),
            1,
        )
        .await;
        if let Some(value) = &admin_value {
            set_preference(&pool, ADMIN, IMAGE_PREFERENCE, value, 1).await;
        }

        run_migration(&pool, 48).await;

        let settings = global_settings(&pool).await;
        assert!(
            settings.is_empty(),
            "admin preference {admin_value:?} must not produce a global setting, got {settings:?}"
        );
    }
}

#[tokio::test]
async fn global_settings_is_a_key_value_table() {
    let pool = pool_at_047().await;
    run_migration(&pool, 48).await;

    let columns: Vec<(String, String, bool, bool)> = sqlx::query("PRAGMA table_info(global_settings)")
        .fetch_all(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|row| {
            (
                row.get("name"),
                row.get("type"),
                row.get::<i64, _>("notnull") == 1,
                row.get::<i64, _>("pk") == 1,
            )
        })
        .collect();
    assert_eq!(
        columns,
        vec![
            ("key".to_owned(), "TEXT".to_owned(), false, true),
            ("value".to_owned(), "TEXT".to_owned(), true, false),
            ("updated_at".to_owned(), "INTEGER".to_owned(), true, false),
        ]
    );

    sqlx::query("INSERT INTO global_settings (key, value, updated_at) VALUES ('k', '{}', 1)")
        .execute(&pool)
        .await
        .unwrap();
    let duplicate = sqlx::query("INSERT INTO global_settings (key, value, updated_at) VALUES ('k', '{}', 2)")
        .execute(&pool)
        .await;
    assert!(duplicate.is_err(), "one value per key");
}

#[tokio::test]
async fn a_fresh_database_starts_with_no_global_settings() {
    let db = aionui_db::init_database_memory().await.unwrap();
    let pool = db.pool().clone();

    let settings = global_settings(&pool).await;
    assert!(settings.is_empty(), "expected no global settings, got {settings:?}");
}
