use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Persistent, revocable capability for a single conversation's public history.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ConversationShareRow {
    pub conversation_id: String,
    pub token: String,
    pub enabled: bool,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}
