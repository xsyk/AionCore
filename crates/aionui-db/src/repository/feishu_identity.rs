use aionui_common::TimestampMs;

use crate::error::DbError;
use crate::models::{FeishuIdentityTransactionKind, FeishuIdentityTransactionRow};

/// Exact context that must still match when a verified assertion is admitted.
#[derive(Clone)]
pub struct ConsumeFeishuIdentityTransactionParams {
    pub transaction_id: String,
    pub node_id: String,
    pub kind: FeishuIdentityTransactionKind,
    pub nonce_hash: String,
    pub browser_binding_hash: String,
    pub redirect_uri: String,
    pub now_ms: TimestampMs,
}

#[async_trait::async_trait]
pub trait IFeishuIdentityTransactionRepository: Send + Sync {
    /// Insert only. Reusing an identifier must never replace its binding.
    async fn insert(&self, row: &FeishuIdentityTransactionRow) -> Result<(), DbError>;

    async fn get(&self, transaction_id: &str) -> Result<Option<FeishuIdentityTransactionRow>, DbError>;

    /// Atomically checks the complete context, expiry and unconsumed state.
    /// Returns false for absent, mismatched, expired or already consumed rows.
    async fn consume(&self, params: &ConsumeFeishuIdentityTransactionParams) -> Result<bool, DbError>;

    async fn delete_expired(&self, now_ms: TimestampMs) -> Result<u64, DbError>;
}
