use std::collections::HashMap;
use std::sync::Arc;

use aionui_common::now_ms;
use aionui_db::models::FeishuGatewayRouteRow;
use serde_json::json;
use tracing::{info, warn};

use super::FeishuBridge;
use crate::error::ChannelError;
use crate::gateway::{GatewayDelivery, GatewayEvent, GatewayRoute};
use crate::types::{
    ActionCategory, ActionContext, MessageContentType, PluginType, UnifiedAction, UnifiedIncomingMessage,
    UnifiedMessageContent, UnifiedUser,
};

pub(super) fn validate_gateway_delivery(
    node: &str,
    app: &str,
    tenant: &str,
    delivery: &GatewayDelivery,
) -> Result<(), ChannelError> {
    let route = &delivery.route;
    if delivery.id.is_empty()
        || route.id.is_empty()
        || route.generation.is_empty()
        || route.conversation_id.is_empty()
        || route.node_id != node
        || route.app_id != app
        || route.tenant_key != tenant
        || route.chat_id.is_empty()
        || route.owner_union_id.is_empty()
        || !matches!(route.chat_type.as_str(), "p2p" | "group")
        || !matches!(
            delivery.kind.as_str(),
            "binding.prepare" | "binding.revoke" | "message" | "interaction"
        )
        || (delivery.kind == "binding.prepare" && !matches!(route.status.as_str(), "preparing" | "active"))
        || (delivery.kind == "binding.revoke" && !matches!(route.status.as_str(), "revoking" | "revoked"))
        || (matches!(delivery.kind.as_str(), "message" | "interaction")
            && (route.status != "active" || delivery.expires_at <= 0))
    {
        return Err(ChannelError::UserNotAuthorized(
            "Gateway delivery identity mismatch".into(),
        ));
    }
    if let Some(event) = &delivery.event {
        if event.id.is_empty()
            || event.app_id != app
            || event.tenant_key != tenant
            || event.chat_id != route.chat_id
            || event.actor_type != "user"
            || event.actor_open_id.is_empty()
            || event.occurred_at <= 0
            || (!event.chat_type.is_empty() && event.chat_type != route.chat_type)
            || (delivery.kind == "message"
                && (event.kind != "message"
                    || event.text.is_empty()
                    || (route.chat_type == "group" && !event.mentioned_bot)))
            || (delivery.kind == "interaction"
                && (event.kind != "interaction"
                    || event.interaction_id.is_empty()
                    || if event.action == "answer" {
                        gateway_text_answer_command(event).is_err()
                            || (route.chat_type == "group" && !event.mentioned_bot)
                    } else {
                        event.platform_message_id.is_empty()
                    }))
        {
            return Err(ChannelError::UserNotAuthorized(
                "Gateway event identity mismatch".into(),
            ));
        }
    } else if matches!(delivery.kind.as_str(), "message" | "interaction") {
        return Err(ChannelError::InvalidConfig("Gateway event is missing".into()));
    }
    Ok(())
}

// Go recognizes the literal "/answer " prefix, then strings.Fields selects
// the interaction token. Preserve answer spacing for the existing question
// parser while checking that it cannot consume a different pending card.
fn gateway_text_answer_command(event: &GatewayEvent) -> Result<&str, ChannelError> {
    let command = event
        .text
        .trim()
        .strip_prefix("/answer ")
        .map(str::trim_start)
        .ok_or_else(|| ChannelError::InvalidConfig("Gateway text answer is invalid".into()))?;
    let (token, answer) = command
        .split_once(char::is_whitespace)
        .ok_or_else(|| ChannelError::InvalidConfig("Gateway text answer is incomplete".into()))?;
    if token != event.interaction_id || answer.trim().is_empty() {
        return Err(ChannelError::UserNotAuthorized(
            "Gateway text answer token or content mismatch".into(),
        ));
    }
    Ok(command)
}

pub(super) fn gateway_message(event: &GatewayEvent, owner: &str) -> Result<UnifiedIncomingMessage, ChannelError> {
    let action = if event.kind == "interaction" && event.action != "answer" {
        let (action, mut params) = if event.action.contains(':') {
            let (_, action, params) = crate::plugins::lark::parse_lark_callback(&event.action)
                .ok_or_else(|| ChannelError::InvalidConfig("Gateway callback action is invalid".into()))?;
            (action, params.unwrap_or_default())
        } else {
            let params = event
                .value
                .get("value")
                .and_then(|value| value.get("params"))
                .and_then(|params| params.as_object())
                .map(|params| {
                    params
                        .iter()
                        .filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_owned())))
                        .collect::<HashMap<_, _>>()
                })
                .unwrap_or_default();
            (event.action.clone(), params)
        };
        if event.interaction_id.is_empty() || params.get("token").is_some_and(|token| token != &event.interaction_id) {
            return Err(ChannelError::UserNotAuthorized(
                "Gateway callback token mismatch".into(),
            ));
        }
        params.insert("token".into(), event.interaction_id.clone());
        Some(UnifiedAction {
            action,
            category: ActionCategory::System,
            params: Some(params),
            context: ActionContext {
                platform: PluginType::Lark,
                user_id: event.actor_open_id.clone(),
                chat_id: event.chat_id.clone(),
                message_id: Some(event.platform_message_id.clone()),
                session_id: None,
            },
        })
    } else {
        None
    };
    Ok(UnifiedIncomingMessage {
        owner_user_id: Some(owner.into()),
        id: event.id.clone(),
        platform: PluginType::Lark,
        chat_id: event.chat_id.clone(),
        user: UnifiedUser {
            id: event.actor_open_id.clone(),
            username: None,
            display_name: event.actor_open_id.clone(),
            avatar_url: None,
        },
        content: UnifiedMessageContent {
            content_type: if action.is_some() {
                MessageContentType::Action
            } else {
                MessageContentType::Text
            },
            text: event.text.clone(),
            attachments: None,
        },
        timestamp: event.occurred_at,
        reply_to_message_id: None,
        action,
        raw: Some(json!({"gateway_action":event.value})),
    })
}

impl FeishuBridge {
    pub(super) async fn flush_gateway(&self) -> Result<(), ChannelError> {
        let gateway = self
            .deps
            .gateway
            .as_ref()
            .ok_or_else(|| ChannelError::InvalidConfig("Gateway is not configured".into()))?;
        let repository = self.gateway_repository()?;
        repository
            .ensure_identity(gateway.node_id(), gateway.app_id(), gateway.tenant_key())
            .await?;
        if self.gateway_auth_rejected.load(std::sync::atomic::Ordering::SeqCst) {
            self.fence_gateway_routes().await?;
        }
        self.restore_gateway_interactions().await?;
        match gateway.deliveries().await {
            Ok(deliveries) => {
                if self
                    .gateway_auth_rejected
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    info!(
                        code = "GATEWAY_AUTH_RESTORED",
                        "feishu: gateway authentication restored; previous generations remain fenced"
                    );
                }
                for delivery in deliveries {
                    if let Err(error) = self.accept_gateway_delivery(&delivery).await {
                        if self.note_gateway_auth_error(&error) {
                            self.fence_gateway_routes().await?;
                            return Err(error);
                        }
                        warn!(delivery_id = %delivery.id, kind = %delivery.kind, code = "GATEWAY_DELIVERY_REJECTED", "feishu: gateway delivery retained for retry");
                    }
                }
            }
            Err(error) => {
                if self.note_gateway_auth_error(&error) {
                    self.fence_gateway_routes().await?;
                    return Err(error);
                }
                warn!(code = "GATEWAY_POLL_FAILED", "feishu: gateway poll will retry");
            }
        }
        // Accepted work on healthy routes must progress before slow control
        // reconciliation for an unavailable or previously fenced generation.
        self.process_gateway_deliveries().await?;
        if let Err(error) = self.reconcile_gateway_routes().await {
            if self.note_gateway_auth_error(&error) {
                self.fence_gateway_routes().await?;
                return Err(error);
            }
            warn!(
                code = "GATEWAY_RECONCILE_FAILED",
                "feishu: gateway route reconciliation will retry"
            );
        }
        Ok(())
    }

    async fn reconcile_gateway_routes(&self) -> Result<(), ChannelError> {
        let repository = self.gateway_repository()?;
        let mut candidates = repository.list_routes().await?;
        {
            let settled = self.gateway_settled_routes.lock().await;
            candidates.retain(|route| {
                matches!(route.status.as_str(), "prepared" | "fenced") && !settled.contains(&route.generation)
            });
        }
        if candidates.is_empty() {
            return Ok(());
        }
        candidates.sort_by(|left, right| left.generation.cmp(&right.generation));
        // At most one bounded HTTP control check each tick; an unavailable
        // generation cannot monopolize all later controls or healthy inputs.
        let local = {
            let mut cursor = self.gateway_reconcile_cursor.lock().await;
            let index = *cursor % candidates.len();
            *cursor = index.wrapping_add(1);
            candidates.swap_remove(index)
        };
        let gateway = self
            .deps
            .gateway
            .as_ref()
            .ok_or_else(|| ChannelError::InvalidConfig("Gateway is not configured".into()))?;
        let _management = self.binding_management_lock.lock().await;
        let lock = self
            .conversation_locks
            .entry(local.conversation_id.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;
        let Some(current_local) = repository.route_by_generation(&local.generation).await? else {
            return Ok(());
        };
        let Some(route) = gateway.route(&local.conversation_id).await? else {
            if current_local.status == "fenced" {
                self.gateway_settled_routes.lock().await.insert(local.generation);
            }
            return Ok(());
        };
        if route.id != local.route_id || route.generation != local.generation {
            repository.fence_route(&local.route_id, &local.generation).await?;
            self.interactions
                .lock()
                .await
                .retain(|_, pending| pending.binding.id != local.generation);
            self.gateway_settled_routes.lock().await.insert(local.generation);
            return Ok(());
        }
        if !same_local_route_identity(&local, &route) {
            return Err(ChannelError::UserNotAuthorized("Gateway route identity changed".into()));
        }
        if route.status == "revoked" {
            repository.fence_route(&local.route_id, &local.generation).await?;
            self.interactions
                .lock()
                .await
                .retain(|_, pending| pending.binding.id != local.generation);
            self.gateway_settled_routes.lock().await.insert(local.generation);
            return Ok(());
        }
        if current_local.status == "fenced" {
            // Authentication recovery never revives an old local generation.
            // Ask the hub to revoke it and let its real control delivery ACK
            // close the remote route. Failed requests retry from the DB fence.
            match route.status.as_str() {
                "active" => {
                    gateway.revoke(&route.id).await?;
                }
                "preparing" if route.prepared_at.is_none() => {
                    gateway.reject(&route.id).await?;
                }
                _ => {}
            }
            return Ok(());
        }
        if route.status != "active" {
            return Ok(());
        }
        match self.gateway_route_owner(&route).await {
            Ok(owner) if owner == local.owner_user_id => {
                repository.activate_route(&route.id, &route.generation).await?;
                info!(route_id = %route.id, generation = %route.generation, "feishu: gateway binding activated");
            }
            Ok(_) => {
                self.fence_known_gateway_route(&route).await?;
                gateway.revoke(&route.id).await?;
            }
            Err(error) if preparation_rejected(&error) => {
                self.fence_known_gateway_route(&route).await?;
                gateway.revoke(&route.id).await?;
                info!(route_id = %route.id, generation = %route.generation, code = "GATEWAY_ACTIVE_OWNER_UNAVAILABLE", "feishu: unavailable prepared route fenced and revocation requested");
            }
            Err(error) => return Err(error),
        }
        Ok(())
    }

    pub(super) async fn accept_gateway_delivery(&self, delivery: &GatewayDelivery) -> Result<(), ChannelError> {
        let gateway = self
            .deps
            .gateway
            .as_ref()
            .ok_or_else(|| ChannelError::InvalidConfig("Gateway is not configured".into()))?;
        validate_gateway_delivery(gateway.node_id(), gateway.app_id(), gateway.tenant_key(), delivery)?;
        let route = &delivery.route;
        let repository = self.gateway_repository()?;
        if matches!(delivery.kind.as_str(), "binding.prepare" | "binding.revoke") {
            let _management = self.binding_management_lock.lock().await;
            let lock = self
                .conversation_locks
                .entry(route.conversation_id.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone();
            let _guard = lock.lock().await;
            if delivery.kind == "binding.prepare" {
                let current = gateway
                    .route(&route.conversation_id)
                    .await?
                    .ok_or_else(|| ChannelError::UserNotAuthorized("Gateway preparation no longer exists".into()))?;
                if !same_route_identity(route, &current) {
                    return Err(ChannelError::UserNotAuthorized(
                        "Gateway preparation differs from authoritative route".into(),
                    ));
                }
                if current.status == "revoked" {
                    self.fence_known_gateway_route(route).await?;
                    return Ok(());
                }
                if !matches!(current.status.as_str(), "preparing" | "active") {
                    return Err(ChannelError::UserNotAuthorized(
                        "Gateway preparation is no longer current".into(),
                    ));
                }
                let owner = match self.gateway_route_owner(route).await {
                    Ok(owner) => owner,
                    Err(error) if preparation_rejected(&error) => {
                        if current.status == "active" {
                            self.fence_known_gateway_route(route).await?;
                            gateway.revoke(&route.id).await?;
                        } else {
                            gateway.reject(&route.id).await?;
                            self.fence_known_gateway_route(route).await?;
                        }
                        info!(route_id = %route.id, generation = %route.generation, code = "GATEWAY_PREPARATION_REJECTED", "feishu: unavailable conversation preparation rejected");
                        return Ok(());
                    }
                    Err(error) => return Err(error),
                };
                repository
                    .prepare_route(&FeishuGatewayRouteRow {
                        route_id: route.id.clone(),
                        generation: route.generation.clone(),
                        node_id: route.node_id.clone(),
                        conversation_id: route.conversation_id.clone(),
                        owner_user_id: owner,
                        owner_union_id: route.owner_union_id.clone(),
                        app_id: route.app_id.clone(),
                        tenant_key: route.tenant_key.clone(),
                        chat_id: route.chat_id.clone(),
                        chat_type: route.chat_type.clone(),
                        status: "prepared".into(),
                        prepared_at: now_ms(),
                    })
                    .await?;
                gateway.ack(&delivery.id, "prepared").await?;
                info!(route_id = %route.id, generation = %route.generation, "feishu: gateway binding prepared");
            } else {
                let local = repository
                    .route_by_generation(&route.generation)
                    .await?
                    .filter(|local| {
                        local.route_id == route.id
                            && local.node_id == route.node_id
                            && local.conversation_id == route.conversation_id
                    })
                    .ok_or_else(|| {
                        ChannelError::UserNotAuthorized("Gateway revoke does not match local generation".into())
                    })?;
                repository.fence_route(&local.route_id, &local.generation).await?;
                self.interactions
                    .lock()
                    .await
                    .retain(|_, pending| pending.binding.id != local.generation);
                gateway.ack(&delivery.id, "revoked").await?;
                info!(route_id = %route.id, generation = %route.generation, "feishu: gateway binding revoked");
            }
            return Ok(());
        }
        let local = repository
            .route_by_generation(&route.generation)
            .await?
            .filter(|local| {
                local.status == "active" && local.route_id == route.id && local.conversation_id == route.conversation_id
            })
            .ok_or_else(|| ChannelError::UserNotAuthorized("Gateway delivery generation is not active".into()))?;
        if local.node_id != gateway.node_id() {
            return Err(ChannelError::UserNotAuthorized("Gateway node mismatch".into()));
        }
        repository
            .enqueue_delivery(
                &delivery.id,
                &route.id,
                &route.generation,
                &delivery.kind,
                &serde_json::to_string(delivery)?,
            )
            .await?;
        gateway.ack(&delivery.id, "accepted").await?;
        Ok(())
    }

    async fn process_gateway_deliveries(&self) -> Result<(), ChannelError> {
        self.ensure_gateway_identity().await?;
        let repository = self.gateway_repository()?;
        let gateway = self
            .deps
            .gateway
            .as_ref()
            .ok_or_else(|| ChannelError::InvalidConfig("Gateway is not configured".into()))?;
        for row in repository.pending_deliveries().await? {
            self.ensure_gateway_identity().await?;
            let delivery: GatewayDelivery = serde_json::from_str(&row.payload)?;
            validate_gateway_delivery(gateway.node_id(), gateway.app_id(), gateway.tenant_key(), &delivery)?;
            if row.route_id != delivery.route.id
                || row.generation != delivery.route.generation
                || row.kind != delivery.kind
                || (delivery.expires_at > 0 && delivery.expires_at <= now_ms())
            {
                repository.complete_delivery(&row.id, "expired_or_invalid").await?;
                continue;
            }
            let Some(local) = repository
                .route_by_generation(&row.generation)
                .await?
                .filter(|local| local.status == "active" && local.route_id == row.route_id)
            else {
                repository.complete_delivery(&row.id, "fenced").await?;
                continue;
            };
            let Some(binding) = self
                .deps
                .bindings
                .get_by_conversation(&local.owner_user_id, &local.conversation_id)
                .await?
                .filter(|binding| binding.id == local.generation)
            else {
                repository.complete_delivery(&row.id, "owner_unavailable").await?;
                continue;
            };
            let event = delivery
                .event
                .as_ref()
                .ok_or_else(|| ChannelError::InvalidConfig("Gateway event is missing".into()))?;
            if delivery.kind == "message" {
                let _management = self.binding_management_lock.lock().await;
                let lock = self
                    .conversation_locks
                    .entry(local.conversation_id.clone())
                    .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                    .clone();
                let _guard = lock.lock().await;
                if self
                    .deps
                    .bindings
                    .get_by_conversation(&binding.owner_user_id, &binding.conversation_id)
                    .await?
                    .as_ref()
                    .map(|current| &current.id)
                    != Some(&binding.id)
                {
                    repository.complete_delivery(&row.id, "fenced").await?;
                    continue;
                }
                let origin = format!("feishu:{}:{}", event.app_id, event.id);
                let text = format!("[飞书 · {}]\n{}", event.actor_open_id, event.text.trim());
                self.deps.bindings.enqueue_inbox(&origin, &binding.id, &text).await?;
                repository.complete_delivery(&row.id, "enqueued").await?;
                continue;
            }
            let previous = { self.gateway_attempts.lock().await.get(&row.id).cloned() };
            if let Some(outcome) = previous {
                if outcome == "outcome_unknown" {
                    self.discard_gateway_interaction(&event.interaction_id).await?;
                }
                repository.complete_delivery(&row.id, &outcome).await?;
                self.gateway_attempts.lock().await.remove(&row.id);
                continue;
            }
            if !repository.start_delivery(&row.id, &self.process_epoch).await? {
                self.discard_gateway_interaction(&event.interaction_id).await?;
                repository.complete_delivery(&row.id, "outcome_unknown").await?;
                warn!(delivery_id = %row.id, code = "GATEWAY_INTERACTION_OUTCOME_UNKNOWN", "feishu: interrupted interaction is not executed again");
                continue;
            }
            self.gateway_attempts
                .lock()
                .await
                .insert(row.id.clone(), "outcome_unknown".into());
            let outcome = if event.action == "answer" {
                match gateway_text_answer_command(event) {
                    Ok(answer) => self.handle_text_answer(&binding, answer).await,
                    Err(error) => Err(error),
                }
            } else {
                match gateway_message(event, &binding.owner_user_id) {
                    Ok(message) => match message.action.as_ref() {
                        Some(action) => self.handle_interaction(&binding, &message, action).await,
                        None => Err(ChannelError::InvalidConfig("Gateway action is missing".into())),
                    },
                    Err(error) => Err(error),
                }
            };
            let outcome = if outcome.is_ok() {
                "handled"
            } else {
                self.discard_gateway_interaction(&event.interaction_id).await?;
                warn!(delivery_id = %row.id, code = "GATEWAY_INTERACTION_OUTCOME_UNKNOWN", "feishu: interaction result requires review");
                "outcome_unknown"
            };
            self.gateway_attempts
                .lock()
                .await
                .insert(row.id.clone(), outcome.into());
            repository.complete_delivery(&row.id, outcome).await?;
            self.gateway_attempts.lock().await.remove(&row.id);
        }
        Ok(())
    }

    pub(super) fn note_gateway_auth_error(&self, error: &ChannelError) -> bool {
        let rejected = matches!(error, ChannelError::ConnectionFailed(code) if code == "GATEWAY_AUTH_REJECTED");
        if rejected {
            self.gateway_auth_rejected
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        rejected
    }

    async fn fence_known_gateway_route(&self, route: &GatewayRoute) -> Result<(), ChannelError> {
        if let Some(local) = self
            .gateway_repository()?
            .route_by_generation(&route.generation)
            .await?
        {
            if local.route_id != route.id {
                return Err(ChannelError::UserNotAuthorized(
                    "Gateway generation identity mismatch".into(),
                ));
            }
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

    async fn fence_gateway_routes(&self) -> Result<(), ChannelError> {
        let repository = self.gateway_repository()?;
        let _management = self.binding_management_lock.lock().await;
        for route in repository.list_routes().await? {
            let lock = self
                .conversation_locks
                .entry(route.conversation_id.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone();
            let _guard = lock.lock().await;
            if route.status != "fenced" {
                repository.fence_route(&route.route_id, &route.generation).await?;
                warn!(route_id = %route.route_id, generation = %route.generation, code = "GATEWAY_AUTH_REJECTED", "feishu: local generation fenced after gateway authentication rejection");
            }
            self.interactions
                .lock()
                .await
                .retain(|_, pending| pending.binding.id != route.generation);
        }
        Ok(())
    }
}

fn same_route_identity(left: &GatewayRoute, right: &GatewayRoute) -> bool {
    left.id == right.id
        && left.generation == right.generation
        && left.node_id == right.node_id
        && left.conversation_id == right.conversation_id
        && left.owner_union_id == right.owner_union_id
        && left.app_id == right.app_id
        && left.tenant_key == right.tenant_key
        && left.chat_id == right.chat_id
        && left.chat_type == right.chat_type
}

fn preparation_rejected(error: &ChannelError) -> bool {
    matches!(
        error,
        ChannelError::UserNotAuthorized(_)
            | ChannelError::SessionNotFound(_)
            | ChannelError::UnsupportedConversation(_)
            | ChannelError::InvalidConfig(_)
    )
}

fn same_local_route_identity(local: &FeishuGatewayRouteRow, remote: &GatewayRoute) -> bool {
    local.route_id == remote.id
        && local.generation == remote.generation
        && local.node_id == remote.node_id
        && local.conversation_id == remote.conversation_id
        && local.owner_union_id == remote.owner_union_id
        && local.app_id == remote.app_id
        && local.tenant_key == remote.tenant_key
        && local.chat_id == remote.chat_id
        && local.chat_type == remote.chat_type
}
