//! Feishu (Lark) OAuth web login: configuration service and shared types.
//! HTTP handlers live in [`routes`].

pub mod account;
pub mod client;
pub mod pkce;
pub(crate) mod routes;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use aionui_api_types::{FeishuLoginConfigUpdate, FeishuLoginConfigView};
use aionui_common::{decrypt_string, encrypt_string};
use aionui_db::{FeishuLoginConfigRow, IFeishuLoginRepository};

pub const FEISHU_EXTERNAL_PREFIX: &str = "feishu:";
pub const FEISHU_STATE_COOKIE: &str = "aionui-feishu-state";
pub const CALLBACK_PATH: &str = "/api/auth/feishu/callback";
const DEFAULT_API_BASE: &str = "https://open.feishu.cn";
const DEFAULT_ACCOUNTS_BASE: &str = "https://accounts.feishu.cn";

/// Failure of a Feishu login step. `code()` is the `feishu_error` value the
/// login page receives.
#[derive(Debug, thiserror::Error)]
pub enum FeishuLoginError {
    #[error("feishu login disabled")]
    Disabled,
    #[error("state mismatch")]
    State,
    #[error("authorization cancelled")]
    Cancelled,
    #[error("tenant not allowed")]
    Tenant,
    #[error("account disabled")]
    AccountDisabled,
    #[error("feishu upstream: {0}")]
    Upstream(String),
    #[error("server: {0}")]
    Server(String),
    #[error("invalid config: {0}")]
    Invalid(String),
}

impl FeishuLoginError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::State => "state",
            Self::Cancelled => "cancelled",
            Self::Tenant => "tenant",
            Self::AccountDisabled => "account_disabled",
            Self::Upstream(_) => "upstream",
            Self::Server(_) | Self::Invalid(_) => "server",
        }
    }
}

/// Decrypted, defaulted configuration for one login attempt.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub app_id: String,
    pub app_secret: String,
    pub tenant_key: Option<String>,
    pub redirect_uri: String,
    pub api_base: String,
    pub accounts_base: String,
}

/// Feishu login service: config persistence (secret encrypted at rest),
/// OAuth calls and tenant enforcement.
pub struct FeishuLogin {
    repo: Arc<dyn IFeishuLoginRepository>,
    encryption_key: [u8; 32],
    http: reqwest::Client,
}

fn server(e: impl std::fmt::Display) -> FeishuLoginError {
    FeishuLoginError::Server(e.to_string())
}

/// Trim, drop trailing `/`, require http(s). Empty → `None`.
fn normalize_url(field: &str, raw: Option<&str>) -> Result<Option<String>, FeishuLoginError> {
    let value = raw.unwrap_or_default().trim().trim_end_matches('/');
    if value.is_empty() {
        return Ok(None);
    }
    match reqwest::Url::parse(value) {
        Ok(url) if matches!(url.scheme(), "http" | "https") && url.host().is_some() => Ok(Some(value.to_owned())),
        _ => Err(FeishuLoginError::Invalid(format!("{field} must be an http(s) URL"))),
    }
}

fn callback_url(public_base_url: &str) -> String {
    if public_base_url.is_empty() {
        String::new()
    } else {
        format!("{public_base_url}{CALLBACK_PATH}")
    }
}

impl FeishuLogin {
    pub fn new(repo: Arc<dyn IFeishuLoginRepository>, encryption_key: [u8; 32], http: reqwest::Client) -> Self {
        Self {
            repo,
            encryption_key,
            http,
        }
    }

    async fn row(&self) -> Result<FeishuLoginConfigRow, FeishuLoginError> {
        Ok(self.repo.get().await.map_err(server)?.unwrap_or_default())
    }

    fn to_view(row: &FeishuLoginConfigRow) -> FeishuLoginConfigView {
        FeishuLoginConfigView {
            enabled: row.enabled,
            app_id: row.app_id.clone(),
            app_secret_set: row.app_secret_enc.is_some(),
            tenant_key: row.tenant_key.clone(),
            public_base_url: row.public_base_url.clone(),
            api_base: row.api_base.clone(),
            accounts_base: row.accounts_base.clone(),
            callback_url: callback_url(&row.public_base_url),
        }
    }

    pub async fn view(&self) -> Result<FeishuLoginConfigView, FeishuLoginError> {
        Ok(Self::to_view(&self.row().await?))
    }

    pub async fn update(&self, req: FeishuLoginConfigUpdate) -> Result<FeishuLoginConfigView, FeishuLoginError> {
        let current = self.row().await?;
        let app_secret_enc = match req.app_secret.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(secret) => Some(encrypt_string(secret, &self.encryption_key).map_err(server)?),
            None => current.app_secret_enc.clone(),
        };
        let public_base_url = normalize_url("public_base_url", Some(&req.public_base_url))?.unwrap_or_default();
        let row = FeishuLoginConfigRow {
            enabled: req.enabled,
            app_id: req.app_id.trim().to_owned(),
            app_secret_enc,
            tenant_key: if req.clear_tenant_key {
                None
            } else {
                current.tenant_key.clone()
            },
            public_base_url,
            api_base: normalize_url("api_base", req.api_base.as_deref())?,
            accounts_base: normalize_url("accounts_base", req.accounts_base.as_deref())?,
            signup_policy: current.signup_policy,
            updated_at: 0,
        };
        if row.enabled && (row.app_id.is_empty() || row.app_secret_enc.is_none() || row.public_base_url.is_empty()) {
            return Err(FeishuLoginError::Invalid(
                "app_id, app_secret and public_base_url are required to enable Feishu login".into(),
            ));
        }
        self.repo.save(&row).await.map_err(server)?;
        self.view().await
    }

    /// Usable configuration, or `Disabled` when off / incomplete / undecryptable.
    pub async fn resolved(&self) -> Result<ResolvedConfig, FeishuLoginError> {
        let row = self.row().await?;
        let Some(secret_enc) = row.app_secret_enc.as_deref().filter(|_| row.enabled) else {
            return Err(FeishuLoginError::Disabled);
        };
        if row.app_id.is_empty() || row.public_base_url.is_empty() {
            return Err(FeishuLoginError::Disabled);
        }
        let app_secret = decrypt_string(secret_enc, &self.encryption_key).map_err(|e| {
            tracing::error!(error = %e, "feishu: stored app secret cannot be decrypted");
            FeishuLoginError::Disabled
        })?;
        Ok(ResolvedConfig {
            app_secret,
            tenant_key: row.tenant_key,
            redirect_uri: callback_url(&row.public_base_url),
            api_base: row.api_base.unwrap_or_else(|| DEFAULT_API_BASE.to_owned()),
            accounts_base: row.accounts_base.unwrap_or_else(|| DEFAULT_ACCOUNTS_BASE.to_owned()),
            app_id: row.app_id,
        })
    }

    pub fn authorize_url(cfg: &ResolvedConfig, state: &str, code_challenge: &str) -> String {
        let base = format!("{}/open-apis/authen/v1/authorize", cfg.accounts_base);
        reqwest::Url::parse_with_params(
            &base,
            &[
                ("client_id", cfg.app_id.as_str()),
                ("response_type", "code"),
                ("redirect_uri", cfg.redirect_uri.as_str()),
                ("state", state),
                ("code_challenge", code_challenge),
                ("code_challenge_method", "S256"),
            ],
        )
        .map(String::from)
        .unwrap_or(base)
    }

    pub async fn exchange_and_fetch(
        &self,
        cfg: &ResolvedConfig,
        code: &str,
        code_verifier: &str,
    ) -> Result<client::FeishuUser, FeishuLoginError> {
        let token = client::exchange_code(
            &self.http,
            &cfg.accounts_base,
            &client::TokenRequest {
                app_id: &cfg.app_id,
                app_secret: &cfg.app_secret,
                code,
                redirect_uri: &cfg.redirect_uri,
                code_verifier,
            },
        )
        .await?;
        client::fetch_user(&self.http, &cfg.api_base, &token).await
    }

    /// Enforce the allowed tenant; the first successful login locks it when unset.
    pub async fn check_tenant(&self, cfg: &ResolvedConfig, tenant_key: &str) -> Result<(), FeishuLoginError> {
        match cfg.tenant_key.as_deref() {
            Some(allowed) if allowed == tenant_key => Ok(()),
            Some(_) => Err(FeishuLoginError::Tenant),
            None => {
                self.repo.set_tenant_key(Some(tenant_key)).await.map_err(server)?;
                tracing::info!("feishu: tenant locked on first login");
                Ok(())
            }
        }
    }
}
