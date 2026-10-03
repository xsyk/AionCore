use crate::error::DbError;
use crate::models::FeishuLoginConfigRow;

/// Single-row Feishu web-login configuration (`feishu_login_config`, id = 1).
/// The app secret arrives already encrypted; callers own encryption.
#[async_trait::async_trait]
pub trait IFeishuLoginRepository: Send + Sync {
    async fn get(&self) -> Result<Option<FeishuLoginConfigRow>, DbError>;

    /// Upserts the whole row; `updated_at` is set to now.
    async fn save(&self, row: &FeishuLoginConfigRow) -> Result<(), DbError>;

    /// Locks (`Some`) or unlocks (`None`) the allowed tenant. No-op without a row.
    async fn set_tenant_key(&self, tenant_key: Option<&str>) -> Result<(), DbError>;
}
