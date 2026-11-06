//! Node-authenticated gateway identity protocol. Credential-bearing DTOs do not implement Debug.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FeishuIdentityKind {
    Oauth,
    H5,
}

impl FeishuIdentityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Oauth => "oauth",
            Self::H5 => "h5",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct FeishuGatewayIdentityBeginRequest {
    pub kind: FeishuIdentityKind,
    pub redirect_uri: String,
    pub nonce: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct FeishuGatewayIdentityTransaction {
    pub transaction_id: String,
    pub expires_at: i64,
    pub app_id: String,
    #[serde(default)]
    pub authorization_url: Option<String>,
    pub verification_key: String,
    pub key_id: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct FeishuGatewayIdentityBeginResponse {
    pub transaction: FeishuGatewayIdentityTransaction,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct FeishuGatewayIdentityExchangeRequest {
    pub transaction_id: String,
    pub code: String,
    pub nonce: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeishuGatewayIdentity {
    pub union_id: String,
    #[serde(default)]
    pub open_id: String,
    pub tenant_key: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub avatar_url: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct FeishuGatewayIdentityExchange {
    /// Untrusted convenience projection. Consumers must use the signed claims instead.
    pub identity: FeishuGatewayIdentity,
    pub assertion: String,
    pub expires_at: i64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct FeishuGatewayIdentityExchangeResponse {
    pub identity: FeishuGatewayIdentityExchange,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct FeishuGatewayIdentityClaims {
    pub iss: String,
    pub aud: String,
    pub sub: String,
    pub tenant_key: String,
    pub iat: i64,
    pub exp: i64,
    pub jti: String,
    pub nonce_hash: String,
    pub identity: FeishuGatewayIdentity,
}
