use serde::{Deserialize, Serialize};

/// Durable local projection of an authenticated gateway route.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct FeishuGatewayRouteRow {
    pub route_id: String,
    pub generation: String,
    pub node_id: String,
    pub conversation_id: String,
    pub owner_user_id: String,
    pub owner_union_id: String,
    pub app_id: String,
    pub tenant_key: String,
    pub chat_id: String,
    pub chat_type: String,
    pub status: String,
    pub prepared_at: i64,
}

/// A committed transport receipt awaiting local application handling.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FeishuGatewayDeliveryRow {
    pub id: String,
    pub route_id: String,
    pub generation: String,
    pub kind: String,
    pub payload: String,
    pub attempt_epoch: Option<String>,
}

/// Complete pending question/permission state, including selected answers.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FeishuGatewayInteractionRow {
    pub token: String,
    pub binding_id: String,
    pub payload: String,
    pub expires_at: i64,
}
