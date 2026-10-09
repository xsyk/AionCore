use sqlx::SqlitePool;

use crate::error::DbError;
use crate::repository::IGlobalSettingRepository;

/// SQLite-backed implementation of [`IGlobalSettingRepository`].
#[derive(Clone, Debug)]
pub struct SqliteGlobalSettingRepository {
    pool: SqlitePool,
}

impl SqliteGlobalSettingRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IGlobalSettingRepository for SqliteGlobalSettingRepository {
    async fn get(&self, key: &str) -> Result<Option<String>, DbError> {
        let value = sqlx::query_scalar::<_, String>("SELECT value FROM global_settings WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;

        Ok(value)
    }

    async fn set(&self, key: &str, value: &str) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO global_settings (key, value, updated_at) VALUES (?, ?, ?) \
             ON CONFLICT(key) DO UPDATE SET \
                value = excluded.value, \
                updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(value)
        .bind(aionui_common::now_ms())
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    const IMAGE_KEY: &str = "tools.imageGeneration";

    async fn setup() -> (SqliteGlobalSettingRepository, crate::Database) {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteGlobalSettingRepository::new(db.pool().clone());
        (repo, db)
    }

    async fn updated_at(db: &crate::Database, key: &str) -> i64 {
        sqlx::query_scalar("SELECT updated_at FROM global_settings WHERE key = ?")
            .bind(key)
            .fetch_one(db.pool())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn get_returns_none_for_a_key_that_was_never_set() {
        let (repo, _db) = setup().await;
        assert_eq!(repo.get(IMAGE_KEY).await.unwrap(), None);
    }

    #[tokio::test]
    async fn set_then_get_returns_the_stored_value() {
        let (repo, _db) = setup().await;
        let value = r#"{"provider_id":"p","model":"gpt-image-1","enabled":true}"#;

        repo.set(IMAGE_KEY, value).await.unwrap();

        assert_eq!(repo.get(IMAGE_KEY).await.unwrap().as_deref(), Some(value));
    }

    #[tokio::test]
    async fn setting_a_key_again_replaces_its_value() {
        let (repo, db) = setup().await;
        repo.set(IMAGE_KEY, "first").await.unwrap();
        repo.set(IMAGE_KEY, "second").await.unwrap();

        assert_eq!(repo.get(IMAGE_KEY).await.unwrap().as_deref(), Some("second"));
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM global_settings WHERE key = ?")
            .bind(IMAGE_KEY)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(rows, 1, "a second set must update the row, not add one");
    }

    #[tokio::test]
    async fn keys_are_independent() {
        let (repo, _db) = setup().await;
        repo.set("a", "1").await.unwrap();
        repo.set("b", "2").await.unwrap();
        repo.set("a", "3").await.unwrap();

        assert_eq!(repo.get("a").await.unwrap().as_deref(), Some("3"));
        assert_eq!(repo.get("b").await.unwrap().as_deref(), Some("2"));
        assert_eq!(repo.get("c").await.unwrap(), None);
    }

    /// An empty value is a value: it must read back as `Some("")`, not as unset.
    #[tokio::test]
    async fn an_empty_value_is_stored_and_read_back_as_is() {
        let (repo, _db) = setup().await;
        repo.set("blank", "").await.unwrap();

        assert_eq!(repo.get("blank").await.unwrap().as_deref(), Some(""));
    }

    #[tokio::test]
    async fn values_are_stored_verbatim() {
        let (repo, _db) = setup().await;
        let value = "line one\nline two \"quoted\" \u{4e2d}\u{6587}  ";

        repo.set("verbatim", value).await.unwrap();

        assert_eq!(repo.get("verbatim").await.unwrap().as_deref(), Some(value));
    }

    #[tokio::test]
    async fn set_stamps_updated_at_and_refreshes_it_on_overwrite() {
        let (repo, db) = setup().await;
        repo.set(IMAGE_KEY, "first").await.unwrap();
        let first = updated_at(&db, IMAGE_KEY).await;
        assert!(first > 0, "updated_at must be a real timestamp, got {first}");

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        repo.set(IMAGE_KEY, "second").await.unwrap();

        assert!(
            updated_at(&db, IMAGE_KEY).await > first,
            "overwriting must refresh updated_at"
        );
    }
}
