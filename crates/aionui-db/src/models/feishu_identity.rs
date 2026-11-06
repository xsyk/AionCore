use std::fmt;

use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum FeishuIdentityTransactionKind {
    Oauth,
    H5,
}

impl FeishuIdentityTransactionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Oauth => "oauth",
            Self::H5 => "h5",
        }
    }
}

/// Local, durable one-shot admission record for a gateway login transaction.
#[derive(Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct FeishuIdentityTransactionRow {
    pub transaction_id: String,
    pub node_id: String,
    pub kind: FeishuIdentityTransactionKind,
    pub nonce_hash: String,
    pub browser_binding_hash: String,
    pub redirect_uri: String,
    pub expires_at: TimestampMs,
    pub consumed_at: Option<TimestampMs>,
}

impl fmt::Debug for FeishuIdentityTransactionRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FeishuIdentityTransactionRow")
            .field("kind", &self.kind)
            .field("expires_at", &self.expires_at)
            .field("consumed_at", &self.consumed_at)
            .finish_non_exhaustive()
    }
}
