use aionui_db::models::FeishuBindingRow;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::FeishuBridge;
use crate::error::ChannelError;
use crate::gateway::{GatewayInteraction, GatewayOutboundRequest};
use crate::types::UnifiedOutgoingMessage;

/// Keep the platform card layout while preserving structured callback values.
pub(super) fn gateway_card(message: &UnifiedOutgoingMessage) -> Result<Value, ChannelError> {
    let mut card: Value = serde_json::from_str(&crate::plugins::lark::build_interactive_card(
        message.text.as_deref().unwrap_or_default(),
        message.buttons.as_deref(),
    ))?;
    if let Some(rows) = &message.buttons {
        for (row_index, row) in rows.iter().filter(|row| !row.is_empty()).enumerate() {
            for (index, button) in row.iter().enumerate() {
                let token = button.params.as_ref().and_then(|params| params.get("token"));
                card["elements"][row_index + 1]["actions"][index]["value"] = json!({
                    "action": button.action, "params": button.params, "interaction_id": token,
                });
            }
        }
    }
    card.sort_all_objects();
    Ok(card)
}

impl FeishuBridge {
    pub(super) async fn send_gateway_message(
        &self,
        binding: &FeishuBindingRow,
        message: UnifiedOutgoingMessage,
        key: Option<&str>,
        interaction: Option<GatewayInteraction>,
    ) -> Result<String, ChannelError> {
        self.ensure_gateway_identity().await?;
        let gateway = self
            .deps
            .gateway
            .as_ref()
            .ok_or_else(|| ChannelError::InvalidConfig("Gateway is not configured".into()))?;
        let repository = self.gateway_repository()?;
        let route = repository
            .route_by_generation(&binding.id)
            .await?
            .filter(|route| {
                route.status == "active"
                    && route.conversation_id == binding.conversation_id
                    && route.owner_user_id == binding.owner_user_id
                    && route.chat_id == binding.chat_id
                    && route.node_id == gateway.node_id()
                    && route.app_id == gateway.app_id()
                    && route.tenant_key == gateway.tenant_key()
            })
            .ok_or_else(|| ChannelError::UserNotAuthorized("Gateway binding is no longer active".into()))?;
        let card = if message.buttons.is_some() {
            Some(gateway_card(&message)?)
        } else {
            None
        };
        let request = GatewayOutboundRequest {
            idempotency_key: key
                .map(str::to_owned)
                .unwrap_or_else(|| format!("reply-{}", aionui_common::generate_id())),
            route_id: route.route_id,
            generation: binding.id.clone(),
            kind: "send".into(),
            text: message.text,
            card,
            interaction,
            ..Default::default()
        };
        gateway.send_and_wait(&request).await.inspect_err(|error| {
            self.note_gateway_auth_error(error);
        })
    }

    pub(super) async fn edit_gateway_message(
        &self,
        binding: &FeishuBindingRow,
        message_id: &str,
        message: UnifiedOutgoingMessage,
    ) -> Result<(), ChannelError> {
        self.ensure_gateway_identity().await?;
        let gateway = self
            .deps
            .gateway
            .as_ref()
            .ok_or_else(|| ChannelError::InvalidConfig("Gateway is not configured".into()))?;
        let route = self
            .gateway_repository()?
            .route_by_generation(&binding.id)
            .await?
            .filter(|route| {
                route.status == "active"
                    && route.conversation_id == binding.conversation_id
                    && route.owner_user_id == binding.owner_user_id
                    && route.chat_id == binding.chat_id
                    && route.node_id == gateway.node_id()
            })
            .ok_or_else(|| ChannelError::UserNotAuthorized("Gateway binding is no longer active".into()))?;
        let card = gateway_card(&message)?;
        let mut digest = Sha256::new();
        digest.update(message_id.as_bytes());
        digest.update([0]);
        digest.update(serde_json::to_vec(&card)?);
        let suffix: String = digest.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
        let request = GatewayOutboundRequest {
            idempotency_key: format!("edit-{suffix}"),
            route_id: route.route_id,
            generation: binding.id.clone(),
            kind: "edit".into(),
            card: Some(card),
            target_message_id: Some(message_id.into()),
            ..Default::default()
        };
        gateway.send_and_wait(&request).await.inspect_err(|error| {
            self.note_gateway_auth_error(error);
        })?;
        Ok(())
    }
}
