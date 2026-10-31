use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuConversationBinding {
    pub chat_id: String,
    pub chat_type: String,
    pub bound_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuBindingStatus {
    pub binding: Option<FeishuConversationBinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuBindingCode {
    pub bind_code: String,
    pub expires_at: i64,
}
