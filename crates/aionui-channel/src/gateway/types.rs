use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GatewayRoute {
    pub id: String,
    pub generation: String,
    pub node_id: String,
    pub conversation_id: String,
    pub owner_union_id: String,
    pub app_id: String,
    pub tenant_key: String,
    pub chat_id: String,
    pub chat_type: String,
    pub status: String,
    pub created_at: i64,
    #[serde(default)]
    pub previous_route_id: Option<String>,
    #[serde(default)]
    pub prepared_at: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GatewayEvent {
    pub id: String,
    pub kind: String,
    pub app_id: String,
    pub tenant_key: String,
    #[serde(default)]
    pub actor_union_id: String,
    #[serde(default)]
    pub actor_open_id: String,
    #[serde(default)]
    pub actor_type: String,
    pub chat_id: String,
    #[serde(default)]
    pub chat_type: String,
    #[serde(default)]
    pub mentioned_bot: bool,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub interaction_id: String,
    #[serde(default)]
    pub platform_message_id: String,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub value: Value,
    pub occurred_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GatewayDelivery {
    pub id: String,
    pub kind: String,
    pub route: GatewayRoute,
    #[serde(default)]
    pub event: Option<GatewayEvent>,
    pub created_at: i64,
    // Go omits a zero TTL on durable binding control deliveries. Message and
    // interaction deliveries still require a positive TTL at validation.
    #[serde(default)]
    pub expires_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GatewayIntent {
    pub code: String,
    pub node_id: String,
    pub conversation_id: String,
    pub owner_union_id: String,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GatewayInteraction {
    pub id: String,
    pub request_id: String,
    pub kind: String,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GatewayOutboundRequest {
    pub idempotency_key: String,
    pub route_id: String,
    pub generation: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interaction: Option<GatewayInteraction>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GatewayOutbound {
    pub id: String,
    pub state: String,
    #[serde(default)]
    pub node_id: String,
    #[serde(default)]
    pub route_id: String,
    #[serde(default)]
    pub generation: String,
    #[serde(default)]
    pub idempotency_key: String,
    #[serde(default)]
    pub platform_message_id: Option<String>,
    #[serde(default)]
    pub interaction_id: Option<String>,
}
