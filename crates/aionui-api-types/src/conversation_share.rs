use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationShareStatus {
    pub enabled: bool,
    pub token: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateConversationShareRequest {
    pub enabled: bool,
}

/// Public metadata deliberately contains no provider, runtime, workspace or owner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicSharedConversation {
    pub id: String,
    pub name: String,
    pub r#type: String,
    pub created_at: TimestampMs,
    pub modified_at: TimestampMs,
}

#[derive(Debug, Default, Deserialize)]
pub struct PublicShareMessagesQuery {
    pub limit: Option<u32>,
    pub before: Option<String>,
    pub after: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ShareResourceQuery {
    pub path: String,
    #[serde(default)]
    pub download: bool,
}
