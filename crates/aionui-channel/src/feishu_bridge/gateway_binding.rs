use std::sync::Arc;

use aionui_api_types::{FeishuBindingStatus, FeishuConversationBinding};
use aionui_db::{IFeishuGatewayRepository, UserStatus};

use super::FeishuBridge;
use crate::error::ChannelError;
use crate::gateway::GatewayRoute;

impl FeishuBridge {
    pub(super) async fn gateway_binding_active(&self, generation: &str) -> Result<bool, ChannelError> {
        let Some(gateway) = self.deps.gateway.as_ref() else {
            return Ok(true);
        };
        if self.gateway_auth_rejected.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(false);
        }
        Ok(self
            .gateway_repository()?
            .route_by_generation(generation)
            .await?
            .is_some_and(|route| {
                route.status == "active"
                    && route.node_id == gateway.node_id()
                    && route.app_id == gateway.app_id()
                    && route.tenant_key == gateway.tenant_key()
            }))
    }
    pub(super) async fn ensure_gateway_identity(&self) -> Result<(), ChannelError> {
        if self.gateway_auth_rejected.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(ChannelError::ConnectionFailed("GATEWAY_AUTH_REJECTED".into()));
        }
        if let Some(gateway) = &self.deps.gateway
            && !self.gateway_identity_checked.load(std::sync::atomic::Ordering::Acquire)
        {
            self.gateway_repository()?
                .ensure_identity(gateway.node_id(), gateway.app_id(), gateway.tenant_key())
                .await?;
            self.gateway_identity_checked
                .store(true, std::sync::atomic::Ordering::Release);
        }
        Ok(())
    }
    pub(super) fn gateway_repository(&self) -> Result<&Arc<dyn IFeishuGatewayRepository>, ChannelError> {
        self.deps
            .gateway_repository
            .as_ref()
            .ok_or_else(|| ChannelError::InvalidConfig("Gateway persistence is not configured".into()))
    }

    pub(super) async fn gateway_route_owner(&self, route: &GatewayRoute) -> Result<String, ChannelError> {
        let gateway = self
            .deps
            .gateway
            .as_ref()
            .ok_or_else(|| ChannelError::InvalidConfig("Gateway is not configured".into()))?;
        if route.node_id != gateway.node_id()
            || route.app_id != gateway.app_id()
            || route.tenant_key != gateway.tenant_key()
            || route.id.is_empty()
            || route.generation.is_empty()
            || route.owner_union_id.is_empty()
            || !matches!(route.chat_type.as_str(), "p2p" | "group")
            || route.chat_id.is_empty()
        {
            return Err(ChannelError::UserNotAuthorized(
                "Gateway route identity mismatch".into(),
            ));
        }
        let user = self
            .deps
            .users
            .find_live_local_by_external_id(&format!("feishu:{}", route.owner_union_id))
            .await?
            .filter(|user| user.status == UserStatus::Active)
            .ok_or_else(|| ChannelError::UserNotAuthorized("Gateway conversation owner is unavailable".into()))?;
        self.ensure_owner(&user.id, &route.conversation_id).await?;
        let conversation = self
            .deps
            .conversation_repo
            .get(&user.id, &route.conversation_id)
            .await?
            .ok_or_else(|| ChannelError::SessionNotFound("Conversation not found".into()))?;
        if aionui_api_types::TeamSessionBinding::team_id_marker_from_extra_str(&conversation.extra).is_some() {
            return Err(ChannelError::UnsupportedConversation(
                "Team conversations cannot be linked to Feishu".into(),
            ));
        }
        let config = self
            .deps
            .login_config
            .get()
            .await?
            .ok_or_else(|| ChannelError::InvalidConfig("Feishu login is not configured".into()))?;
        if !config.enabled
            || config.app_id != route.app_id
            || config.tenant_key.as_deref() != Some(route.tenant_key.as_str())
        {
            return Err(ChannelError::UserNotAuthorized(
                "Feishu application or enterprise is not allowed".into(),
            ));
        }
        Ok(user.id)
    }

    pub(super) async fn gateway_binding_status(
        &self,
        owner: &str,
        conversation: &str,
    ) -> Result<FeishuBindingStatus, ChannelError> {
        self.ensure_gateway_identity().await?;
        let gateway = self
            .deps
            .gateway
            .as_ref()
            .ok_or_else(|| ChannelError::InvalidConfig("Gateway is not configured".into()))?;
        let Some(route) = gateway.route(conversation).await.inspect_err(|error| {
            self.note_gateway_auth_error(error);
        })?
        else {
            return Ok(FeishuBindingStatus { binding: None });
        };
        if self.gateway_route_owner(&route).await? != owner {
            return Err(ChannelError::UserNotAuthorized(
                "Only the conversation owner may inspect its binding".into(),
            ));
        }
        let repository = self.gateway_repository()?;
        if route.status == "active" {
            let _management = self.binding_management_lock.lock().await;
            let lock = self
                .conversation_locks
                .entry(conversation.to_owned())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone();
            let _guard = lock.lock().await;
            if let Some(local) = repository.route_by_generation(&route.generation).await?
                && local.route_id == route.id
                && local.status == "prepared"
            {
                repository.activate_route(&route.id, &route.generation).await?;
            }
            let binding = self
                .deps
                .bindings
                .get_by_conversation(owner, conversation)
                .await?
                .filter(|binding| binding.id == route.generation)
                .map(|binding| FeishuConversationBinding {
                    chat_id: binding.chat_id,
                    chat_type: binding.chat_type,
                    bound_at: binding.bound_at,
                });
            return Ok(FeishuBindingStatus { binding });
        }
        Ok(FeishuBindingStatus { binding: None })
    }

    pub(super) async fn gateway_unbind(&self, owner: &str, conversation: &str) -> Result<(), ChannelError> {
        self.ensure_gateway_identity().await?;
        self.ensure_owner(owner, conversation).await?;
        let gateway = self
            .deps
            .gateway
            .as_ref()
            .ok_or_else(|| ChannelError::InvalidConfig("Gateway is not configured".into()))?;
        let Some(route) = gateway.route(conversation).await.inspect_err(|error| {
            self.note_gateway_auth_error(error);
        })?
        else {
            return Ok(());
        };
        if self.gateway_route_owner(&route).await? != owner {
            return Err(ChannelError::UserNotAuthorized(
                "Only the conversation owner may unbind".into(),
            ));
        }
        if route.status != "revoked" {
            gateway.revoke(&route.id).await.inspect_err(|error| {
                self.note_gateway_auth_error(error);
            })?;
        }
        let _management = self.binding_management_lock.lock().await;
        let lock = self
            .conversation_locks
            .entry(conversation.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;
        if self
            .gateway_repository()?
            .route_by_generation(&route.generation)
            .await?
            .is_some()
        {
            self.gateway_repository()?
                .fence_route(&route.id, &route.generation)
                .await?;
        }
        self.interactions
            .lock()
            .await
            .retain(|_, pending| pending.binding.id != route.generation);
        Ok(())
    }
}
