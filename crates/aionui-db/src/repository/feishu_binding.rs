use crate::DbError;
use crate::models::{
    FeishuBindingCodeRow, FeishuBindingRow, FeishuInboxRow, FeishuIngressRow, FeishuOutboxRow, MessageRow,
    RedeemFeishuBindingParams,
};

#[async_trait::async_trait]
pub trait IFeishuBindingRepository: Send + Sync {
    async fn get_by_conversation(&self, owner: &str, conversation: &str) -> Result<Option<FeishuBindingRow>, DbError>;
    async fn get_by_chat(&self, app: &str, tenant: &str, chat: &str) -> Result<Option<FeishuBindingRow>, DbError>;
    async fn save_code(&self, code: &FeishuBindingCodeRow) -> Result<(), DbError>;
    async fn redeem_code(&self, params: RedeemFeishuBindingParams<'_>) -> Result<FeishuBindingRow, DbError>;
    async fn delete_binding(&self, owner: &str, conversation: &str) -> Result<bool, DbError>;
    async fn list_bindings(&self) -> Result<Vec<FeishuBindingRow>, DbError>;
    async fn enqueue_inbox(&self, id: &str, binding: &str, content: &str) -> Result<(), DbError>;
    async fn pending_inbox(&self) -> Result<Vec<FeishuInboxRow>, DbError>;
    async fn complete_inbox(&self, id: &str) -> Result<(), DbError>;
    async fn sync_candidates(&self, binding: &str) -> Result<Vec<MessageRow>, DbError>;
    async fn enqueue_outbox(&self, rows: &[FeishuOutboxRow]) -> Result<(), DbError>;
    async fn pending_outbox(&self) -> Result<Vec<FeishuOutboxRow>, DbError>;
    async fn start_outbox_attempt(&self, id: &str, now: i64) -> Result<(), DbError>;
    async fn complete_outbox(&self, id: &str, platform_id: &str) -> Result<(), DbError>;
    async fn get_code(&self, code: &str) -> Result<Option<FeishuBindingCodeRow>, DbError>;
    async fn enqueue_ingress(&self, id: &str, payload: &str) -> Result<(), DbError>;
    async fn pending_ingress(&self) -> Result<Vec<FeishuIngressRow>, DbError>;
    async fn complete_ingress(&self, id: &str) -> Result<(), DbError>;
    async fn start_inbox_attempt(&self, id: &str, epoch: &str) -> Result<(), DbError>;
    async fn park_expired_outbox(&self, before: i64) -> Result<u64, DbError>;
    async fn park_outbox(&self, id: &str) -> Result<(), DbError>;
    async fn claim_message(&self, app: &str, message: &str, now: i64) -> Result<bool, DbError>;
}
