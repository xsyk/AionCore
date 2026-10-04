/// How a first-time Feishu user is admitted (migration 047).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
pub enum FeishuSignupPolicy {
    /// New accounts start disabled until a super admin enables them.
    #[default]
    Approval,
    /// New accounts are active immediately.
    Open,
}

impl FeishuSignupPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::Open => "open",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "approval" => Some(Self::Approval),
            "open" => Some(Self::Open),
            _ => None,
        }
    }
}

/// Row mapping for the single-row `feishu_login_config` table (migrations 046, 047).
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
    pub signup_policy: FeishuSignupPolicy,
    pub updated_at: i64,
}
