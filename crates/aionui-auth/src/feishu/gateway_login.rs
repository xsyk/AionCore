//! Durable admission of node-bound gateway login assertions.
use super::gateway_identity::{FeishuGatewayIdentityBackend, GatewayIdentityError, GatewayIdentityTrust, hash, opaque};
use super::{FeishuLoginError, client::FeishuUser};
use aionui_api_types::{
    FeishuGatewayIdentityBeginRequest, FeishuGatewayIdentityExchangeRequest, FeishuGatewayIdentityTransaction,
    FeishuIdentityKind,
};
use aionui_db::{
    ConsumeFeishuIdentityTransactionParams, FeishuIdentityTransactionKind, FeishuIdentityTransactionRow,
    IFeishuIdentityTransactionRepository,
};
use std::sync::Arc;

pub struct GatewayIdentityLogin {
    trust: GatewayIdentityTrust,
    backend: Arc<dyn FeishuGatewayIdentityBackend>,
    transactions: Arc<dyn IFeishuIdentityTransactionRepository>,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
}

pub struct GatewayLoginAttempt<'a> {
    pub kind: FeishuIdentityKind,
    pub transaction_id: &'a str,
    pub redirect_uri: &'a str,
    pub nonce: &'a str,
    pub browser_binding: &'a str,
    pub code: &'a str,
}

impl GatewayIdentityLogin {
    pub fn new(
        trust: GatewayIdentityTrust,
        backend: Arc<dyn FeishuGatewayIdentityBackend>,
        transactions: Arc<dyn IFeishuIdentityTransactionRepository>,
    ) -> Result<Self, GatewayIdentityError> {
        trust.validate()?;
        Ok(Self {
            trust,
            backend,
            transactions,
            clock: Arc::new(aionui_common::now_ms),
        })
    }

    pub fn trust(&self) -> &GatewayIdentityTrust {
        &self.trust
    }

    pub async fn begin(
        &self,
        kind: FeishuIdentityKind,
        redirect_uri: &str,
        nonce: &str,
        browser_binding: &str,
    ) -> Result<FeishuGatewayIdentityTransaction, FeishuLoginError> {
        if !valid_context(redirect_uri, nonce, browser_binding) {
            return Err(invalid());
        }
        let now_ms = (self.clock)();
        self.transactions.delete_expired(now_ms).await.map_err(|_| storage())?;
        let result = self
            .backend
            .begin(FeishuGatewayIdentityBeginRequest {
                kind,
                redirect_uri: redirect_uri.into(),
                nonce: nonce.into(),
            })
            .await?;
        self.validate_transaction(&result, kind, redirect_uri, (self.clock)())?;
        self.transactions
            .insert(&FeishuIdentityTransactionRow {
                transaction_id: result.transaction_id.clone(),
                node_id: self.trust.node_id.clone(),
                kind: db_kind(kind),
                nonce_hash: hash(nonce.as_bytes()),
                browser_binding_hash: hash(browser_binding.as_bytes()),
                redirect_uri: redirect_uri.into(),
                expires_at: result.expires_at,
                consumed_at: None,
            })
            .await
            .map_err(|_| storage())?;
        Ok(result)
    }

    pub async fn finish(&self, attempt: GatewayLoginAttempt<'_>) -> Result<FeishuUser, FeishuLoginError> {
        let GatewayLoginAttempt {
            kind,
            transaction_id,
            redirect_uri,
            nonce,
            browser_binding,
            code,
        } = attempt;
        let now_ms = (self.clock)();
        if !opaque(transaction_id)
            || !valid_context(redirect_uri, nonce, browser_binding)
            || code.trim().is_empty()
            || code.len() > 4096
        {
            return Err(invalid());
        }
        let mut expected = ConsumeFeishuIdentityTransactionParams {
            transaction_id: transaction_id.into(),
            node_id: self.trust.node_id.clone(),
            kind: db_kind(kind),
            nonce_hash: hash(nonce.as_bytes()),
            browser_binding_hash: hash(browser_binding.as_bytes()),
            redirect_uri: redirect_uri.into(),
            now_ms,
        };
        let row = self
            .transactions
            .get(transaction_id)
            .await
            .map_err(|_| storage())?
            .ok_or(FeishuLoginError::State)?;
        if row.node_id != expected.node_id
            || row.kind != expected.kind
            || row.nonce_hash != expected.nonce_hash
            || row.browser_binding_hash != expected.browser_binding_hash
            || row.redirect_uri != expected.redirect_uri
            || row.expires_at <= now_ms
            || row.consumed_at.is_some()
        {
            return Err(FeishuLoginError::State);
        }
        let result = self
            .backend
            .exchange(FeishuGatewayIdentityExchangeRequest {
                transaction_id: transaction_id.into(),
                code: code.into(),
                nonce: nonce.into(),
            })
            .await?;
        expected.now_ms = (self.clock)();
        let user = self
            .trust
            .verify_assertion(&result, transaction_id, nonce, expected.now_ms)
            .map_err(|_| invalid())?;
        if !self.transactions.consume(&expected).await.map_err(|_| storage())? {
            return Err(FeishuLoginError::State);
        }
        Ok(user)
    }

    fn validate_transaction(
        &self,
        result: &FeishuGatewayIdentityTransaction,
        kind: FeishuIdentityKind,
        redirect_uri: &str,
        now_ms: i64,
    ) -> Result<(), FeishuLoginError> {
        if !opaque(&result.transaction_id) || result.app_id != self.trust.app_id || result.verification_key != self.trust.verification_key || result.key_id != self.trust.key_id
            // A bounded 30s clock skew is allowed; the exact hub deadline is
            // persisted, not extended by the consumer.
            || result.expires_at <= now_ms || result.expires_at > now_ms.saturating_add(630_000)
        {
            return Err(invalid());
        }
        if kind == FeishuIdentityKind::H5 {
            return if result.authorization_url.is_none() {
                Ok(())
            } else {
                Err(invalid())
            };
        }
        let url =
            reqwest::Url::parse(result.authorization_url.as_deref().ok_or_else(invalid)?).map_err(|_| invalid())?;
        if url.scheme() != "https"
            || url.host_str() != Some("accounts.feishu.cn")
            || url.port().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || url.path() != "/open-apis/authen/v1/authorize"
        {
            return Err(invalid());
        }
        let pairs: Vec<_> = url.query_pairs().collect();
        if pairs.len() != 6 {
            return Err(invalid());
        }
        let mut values = std::collections::HashMap::new();
        for (key, value) in pairs {
            if values.insert(key.into_owned(), value.into_owned()).is_some() {
                return Err(invalid());
            }
        }
        let equal = |key: &str, expected: &str| values.get(key).is_some_and(|value| value == expected);
        if !equal("client_id", &self.trust.app_id)
            || !equal("response_type", "code")
            || !equal("redirect_uri", redirect_uri)
            || !equal("state", &result.transaction_id)
            || !equal("code_challenge_method", "S256")
            || !values.get("code_challenge").is_some_and(|value| opaque(value))
        {
            return Err(invalid());
        }
        Ok(())
    }
}

fn db_kind(kind: FeishuIdentityKind) -> FeishuIdentityTransactionKind {
    match kind {
        FeishuIdentityKind::Oauth => FeishuIdentityTransactionKind::Oauth,
        FeishuIdentityKind::H5 => FeishuIdentityTransactionKind::H5,
    }
}
fn valid_context(redirect_uri: &str, nonce: &str, browser: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(redirect_uri) else {
        return false;
    };
    redirect_uri.len() <= 2048
        && url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && url.query().is_none()
        && (32..=256).contains(&nonce.len())
        && (16..=512).contains(&browser.len())
}
fn invalid() -> FeishuLoginError {
    FeishuLoginError::Upstream("FEISHU_GATEWAY_IDENTITY_INVALID".into())
}
fn storage() -> FeishuLoginError {
    FeishuLoginError::Server("FEISHU_GATEWAY_LOGIN_STORE_FAILED".into())
}
