//! Independently pinned gateway identity verification and transport abstraction.
use super::{FeishuLoginError, client::FeishuUser};
use aionui_api_types::FeishuGatewayIdentityClaims;
use aionui_api_types::{
    FeishuGatewayIdentityBeginRequest, FeishuGatewayIdentityExchange, FeishuGatewayIdentityExchangeRequest,
    FeishuGatewayIdentityTransaction,
};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
#[error("{code}")]
pub struct GatewayIdentityError {
    code: &'static str,
}

impl GatewayIdentityError {
    pub fn code(&self) -> &'static str {
        self.code
    }
    pub(crate) fn new(code: &'static str) -> Self {
        Self { code }
    }
}

#[derive(Clone)]
pub struct GatewayIdentityTrust {
    pub node_id: String,
    pub app_id: String,
    pub tenant_key: String,
    pub verification_key: String,
    pub key_id: String,
}

#[async_trait]
pub trait FeishuGatewayIdentityBackend: Send + Sync {
    async fn begin(
        &self,
        request: FeishuGatewayIdentityBeginRequest,
    ) -> Result<FeishuGatewayIdentityTransaction, FeishuLoginError>;
    async fn exchange(
        &self,
        request: FeishuGatewayIdentityExchangeRequest,
    ) -> Result<FeishuGatewayIdentityExchange, FeishuLoginError>;
}

impl GatewayIdentityTrust {
    pub fn validate(&self) -> Result<(), GatewayIdentityError> {
        let invalid = || GatewayIdentityError::new("FEISHU_GATEWAY_TRUST_INVALID");
        for value in [&self.node_id, &self.app_id, &self.tenant_key] {
            if value.is_empty()
                || value.len() > 512
                || value.chars().any(|value| value.is_whitespace() || value.is_control())
            {
                return Err(invalid());
            }
        }
        let bytes = decode_public_key(&self.verification_key).ok_or_else(invalid)?;
        if hash(&bytes) != self.key_id || VerifyingKey::from_bytes(&bytes).is_err() {
            return Err(invalid());
        }
        Ok(())
    }

    pub fn verify_assertion(
        &self,
        result: &FeishuGatewayIdentityExchange,
        transaction_id: &str,
        nonce: &str,
        now_ms: i64,
    ) -> Result<FeishuUser, GatewayIdentityError> {
        let invalid = || GatewayIdentityError::new("FEISHU_GATEWAY_ASSERTION_INVALID");
        self.validate().map_err(|_| invalid())?;
        if result.assertion.len() > 16384
            || !opaque(transaction_id)
            || nonce.len() < 32
            || nonce.len() > 256
            || now_ms <= 0
        {
            return Err(invalid());
        }
        let parts: Vec<_> = result.assertion.split('.').collect();
        if parts.len() != 3 {
            return Err(invalid());
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Header {
            alg: String,
            typ: String,
            kid: String,
        }
        let header: Header =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).map_err(|_| invalid())?).map_err(|_| invalid())?;
        if header.alg != "EdDSA" || header.typ != "JWT" || header.kid != self.key_id {
            return Err(invalid());
        }
        let signature =
            Signature::from_slice(&URL_SAFE_NO_PAD.decode(parts[2]).map_err(|_| invalid())?).map_err(|_| invalid())?;
        let key = VerifyingKey::from_bytes(&decode_public_key(&self.verification_key).ok_or_else(invalid)?)
            .map_err(|_| invalid())?;
        key.verify_strict(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .map_err(|_| invalid())?;
        let claims: FeishuGatewayIdentityClaims =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).map_err(|_| invalid())?).map_err(|_| invalid())?;
        let now = now_ms / 1000;
        if claims.iss != "aion-feishu-route"
            || claims.aud != self.node_id
            || claims.tenant_key != self.tenant_key
            || claims.identity.tenant_key != self.tenant_key
            || claims.identity.union_id.is_empty()
            || claims.identity.union_id.len() > 512
            || claims.sub != format!("feishu:{}", claims.identity.union_id)
            || claims.jti != transaction_id
            || claims.nonce_hash != hash(nonce.as_bytes())
            || claims.exp <= now
            || claims.iat > now.saturating_add(30)
            || claims.iat < 0
            || !matches!(claims.exp.checked_sub(claims.iat), Some(1..=120))
        {
            return Err(invalid());
        }
        // Neither the outer identity nor millisecond expiry is signed. Only
        // admitted claims may determine the local account and assertion TTL.
        Ok(FeishuUser {
            union_id: claims.identity.union_id,
            tenant_key: claims.identity.tenant_key,
            name: claims.identity.name,
            en_name: String::new(),
            email: nonempty(claims.identity.email),
            avatar_url: nonempty(claims.identity.avatar_url),
        })
    }
}

fn nonempty(value: String) -> Option<String> {
    if value.is_empty() { None } else { Some(value) }
}
fn decode_public_key(raw: &str) -> Option<[u8; 32]> {
    let bytes: [u8; 32] = URL_SAFE_NO_PAD.decode(raw).ok()?.try_into().ok()?;
    (URL_SAFE_NO_PAD.encode(bytes) == raw).then_some(bytes)
}
pub(crate) fn hash(value: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(value))
}
pub(crate) fn opaque(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[cfg(test)]
#[path = "gateway_identity_tests.rs"]
mod tests;
