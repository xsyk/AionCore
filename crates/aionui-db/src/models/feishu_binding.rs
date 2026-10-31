use serde::{Deserialize, Serialize};

/// A one-to-one association between a Feishu chat and an existing conversation.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct FeishuBindingRow {
    pub id: String,
    pub conversation_id: String,
    pub owner_user_id: String,
    pub connector_user_id: String,
    pub app_id: String,
    pub tenant_key: String,
    pub chat_id: String,
    pub chat_type: String,
    pub bound_at: i64,
}

/// Short-lived proof of the owner's intent to bind a conversation.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FeishuBindingCodeRow {
    pub code: String,
    pub conversation_id: String,
    pub owner_user_id: String,
    pub expires_at: i64,
}

/// Identity comes exclusively from the authenticated Feishu transport.
pub struct RedeemFeishuBindingParams<'a> {
    pub code: &'a str,
    pub external_user_id: &'a str,
    pub connector_user_id: &'a str,
    pub app_id: &'a str,
    pub tenant_key: &'a str,
    pub chat_id: &'a str,
    pub chat_type: &'a str,
    pub now: i64,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FeishuInboxRow {
    pub id: String,
    pub binding_id: String,
    pub content: String,
    pub attempt_epoch: Option<String>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FeishuOutboxRow {
    pub id: String,
    pub binding_id: String,
    pub message_id: String,
    pub part: i64,
    pub content: String,
    pub platform_message_id: Option<String>,
    pub first_attempt_at: Option<i64>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FeishuIngressRow {
    pub id: String,
    pub payload: String,
}
