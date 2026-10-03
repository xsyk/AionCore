/// Row mapping for the single-row `feishu_login_config` table (migration 046).
///
/// `app_secret_enc` holds the AES-256-GCM encrypted app secret; the
/// repository never sees plaintext. Optional URL columns fall back to the
/// public Feishu endpoints when `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq, sqlx::FromRow)]
pub struct FeishuLoginConfigRow {
    pub enabled: bool,
    pub app_id: String,
    pub app_secret_enc: Option<String>,
    /// Allowed tenant; `None` until the first successful login locks it.
    pub tenant_key: Option<String>,
    pub public_base_url: String,
    pub api_base: Option<String>,
    pub accounts_base: Option<String>,
    pub updated_at: i64,
}
