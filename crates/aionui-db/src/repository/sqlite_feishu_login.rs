use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::FeishuLoginConfigRow;
use crate::repository::feishu_login::IFeishuLoginRepository;

const COLUMNS: &str =
    "enabled, app_id, app_secret_enc, tenant_key, public_base_url, api_base, accounts_base, updated_at";

/// SQLite-backed implementation of [`IFeishuLoginRepository`].
#[derive(Clone, Debug)]
pub struct SqliteFeishuLoginRepository {
    pool: SqlitePool,
}

impl SqliteFeishuLoginRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IFeishuLoginRepository for SqliteFeishuLoginRepository {
    async fn get(&self) -> Result<Option<FeishuLoginConfigRow>, DbError> {
        let row = sqlx::query_as::<_, FeishuLoginConfigRow>(&format!(
            "SELECT {COLUMNS} FROM feishu_login_config WHERE id = 1"
        ))
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn save(&self, row: &FeishuLoginConfigRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO feishu_login_config \
                (id, enabled, app_id, app_secret_enc, tenant_key, public_base_url, api_base, accounts_base, updated_at) \
             VALUES (1, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET \
                enabled = excluded.enabled, app_id = excluded.app_id, app_secret_enc = excluded.app_secret_enc, \
                tenant_key = excluded.tenant_key, public_base_url = excluded.public_base_url, \
                api_base = excluded.api_base, accounts_base = excluded.accounts_base, updated_at = excluded.updated_at",
        )
        .bind(row.enabled)
        .bind(&row.app_id)
        .bind(&row.app_secret_enc)
        .bind(&row.tenant_key)
        .bind(&row.public_base_url)
        .bind(&row.api_base)
        .bind(&row.accounts_base)
        .bind(aionui_common::now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn set_tenant_key(&self, tenant_key: Option<&str>) -> Result<(), DbError> {
        sqlx::query("UPDATE feishu_login_config SET tenant_key = ?, updated_at = ? WHERE id = 1")
            .bind(tenant_key)
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

    #[tokio::test]
    async fn get_save_and_tenant_lock() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteFeishuLoginRepository::new(db.pool().clone());
        assert!(repo.get().await.unwrap().is_none());
        let row = FeishuLoginConfigRow {
            enabled: true,
            app_id: "cli_x".into(),
            app_secret_enc: Some("enc".into()),
            public_base_url: "https://aidi.example.com".into(),
            ..Default::default()
        };
        repo.save(&row).await.unwrap();
        let got = repo.get().await.unwrap().unwrap();
        assert!(got.enabled);
        assert_eq!(got.app_id, "cli_x");
        assert!(got.tenant_key.is_none());
        repo.set_tenant_key(Some("t1")).await.unwrap();
        assert_eq!(repo.get().await.unwrap().unwrap().tenant_key.as_deref(), Some("t1"));
        // save overwrites the whole row (tenant_key included)
        repo.save(&FeishuLoginConfigRow {
            tenant_key: Some("t1".into()),
            ..row.clone()
        })
        .await
        .unwrap();
        repo.set_tenant_key(None).await.unwrap();
        assert!(repo.get().await.unwrap().unwrap().tenant_key.is_none());
    }
}
