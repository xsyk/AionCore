use std::collections::HashMap;
use std::sync::Arc;

use aionui_api_types::{FeishuBindingCode, FeishuBindingStatus, FeishuConversationBinding};
use aionui_common::now_ms;
use aionui_db::UserStatus;
use aionui_db::models::{FeishuBindingCodeRow, FeishuBindingRow, RedeemFeishuBindingParams};
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{info, warn};

use super::state::{FeishuBridge, FeishuBridgeDependencies};
use crate::error::ChannelError;
use crate::types::{OutgoingMessageType, PluginType, UnifiedIncomingMessage, UnifiedOutgoingMessage};

pub(super) const BINDING_TTL_MS: i64 = 10 * 60 * 1000;

/// Identity metadata is added by the authenticated Lark WebSocket plugin, never
/// accepted from a public HTTP request or parsed out of message text.
#[derive(Debug, Clone)]
pub(super) struct FeishuIdentity {
    pub app_id: String,
    pub tenant_key: String,
    pub union_id: String,
    pub chat_type: String,
    pub mentioned_bot: bool,
    pub callback: bool,
}

impl FeishuIdentity {
    pub(super) fn from_message(message: &UnifiedIncomingMessage) -> Option<Self> {
        let raw = message.raw.as_ref()?;
        let transport = raw.get("feishu_transport")?;
        let event = raw.get("feishu_event")?;
        let app_id = transport.get("app_id")?.as_str()?.to_owned();
        let tenant_key = event.get("tenant_key")?.as_str()?.to_owned();
        if app_id.is_empty()
            || tenant_key.is_empty()
            || message.id.is_empty()
            || message.user.id.is_empty()
            || message.chat_id.is_empty()
        {
            return None;
        }
        if event.get("sender_type").and_then(Value::as_str) != Some("user") {
            return None;
        }
        if event
            .get("app_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .is_some_and(|id| id != app_id)
        {
            return None;
        }
        let bot = transport.get("bot_open_id").and_then(Value::as_str).unwrap_or_default();
        let mentioned_bot = !bot.is_empty()
            && event
                .get("mentioned_ids")
                .and_then(Value::as_array)
                .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(bot)));
        Some(Self {
            app_id,
            tenant_key,
            union_id: event
                .get("union_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            chat_type: event
                .get("chat_type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            mentioned_bot,
            callback: message.action.is_some(),
        })
    }

    pub(super) fn may_participate(&self, binding: &FeishuBindingRow) -> bool {
        self.app_id == binding.app_id
            && self.tenant_key == binding.tenant_key
            && (self.callback || binding.chat_type == "p2p" || self.mentioned_bot)
    }
}

impl FeishuBridge {
    pub fn new(deps: FeishuBridgeDependencies) -> Self {
        Self {
            deps,
            interactions: Mutex::new(HashMap::new()),
            delivery_lock: Mutex::new(()),
            binding_management_lock: Mutex::new(()),
            process_epoch: aionui_common::generate_id(),
            conversation_locks: dashmap::DashMap::new(),
        }
    }

    pub(super) async fn ensure_owner(&self, owner: &str, conversation: &str) -> Result<(), ChannelError> {
        let row = self
            .deps
            .conversation_repo
            .get(owner, conversation)
            .await?
            .ok_or_else(|| ChannelError::SessionNotFound("Conversation not found".into()))?;
        if row.user_id != owner {
            return Err(ChannelError::UserNotAuthorized(
                "Only the conversation owner may manage its Feishu binding".into(),
            ));
        }
        Ok(())
    }

    pub async fn status(&self, owner: &str, conversation: &str) -> Result<FeishuBindingStatus, ChannelError> {
        self.ensure_owner(owner, conversation).await?;
        let binding = self.deps.bindings.get_by_conversation(owner, conversation).await?;
        Ok(FeishuBindingStatus {
            binding: binding.map(|row| FeishuConversationBinding {
                chat_id: row.chat_id,
                chat_type: row.chat_type,
                bound_at: row.bound_at,
            }),
        })
    }

    pub async fn create_binding_code(
        &self,
        owner: &str,
        conversation: &str,
    ) -> Result<FeishuBindingCode, ChannelError> {
        self.ensure_owner(owner, conversation).await?;
        let row = self
            .deps
            .conversation_repo
            .get(owner, conversation)
            .await?
            .ok_or_else(|| ChannelError::SessionNotFound("Conversation not found".into()))?;
        if aionui_api_types::TeamSessionBinding::team_id_marker_from_extra_str(&row.extra).is_some() {
            return Err(ChannelError::UnsupportedConversation(
                "团队会话需使用团队运行入口，暂不支持关联飞书；请选择普通会话。".into(),
            ));
        }
        let user = self
            .deps
            .users
            .find_active_by_id(owner)
            .await?
            .filter(|user| {
                user.external_user_id
                    .as_deref()
                    .is_some_and(|id| id.starts_with("feishu:"))
            })
            .ok_or_else(|| ChannelError::UserNotAuthorized("Sign in with Feishu to link a conversation".into()))?;
        if user.status != UserStatus::Active {
            return Err(ChannelError::UserNotAuthorized("Account is disabled".into()));
        }
        self.deps
            .login_config
            .get()
            .await?
            .filter(|row| row.enabled && !row.app_id.is_empty() && row.tenant_key.is_some())
            .ok_or_else(|| {
                ChannelError::InvalidConfig("Configure Feishu login and the allowed enterprise first".into())
            })?;
        let bind_code = random_token()?;
        let expires_at = now_ms() + BINDING_TTL_MS;
        self.deps
            .bindings
            .save_code(&FeishuBindingCodeRow {
                code: bind_code.clone(),
                conversation_id: conversation.into(),
                owner_user_id: owner.into(),
                expires_at,
            })
            .await?;
        info!(
            owner_user_id = owner,
            conversation_id = conversation,
            "feishu: binding requested"
        );
        Ok(FeishuBindingCode { bind_code, expires_at })
    }

    pub async fn unbind(&self, owner: &str, conversation: &str) -> Result<(), ChannelError> {
        self.unbind_generation(owner, conversation, None).await
    }

    async fn unbind_generation(
        &self,
        owner: &str,
        conversation: &str,
        expected: Option<&str>,
    ) -> Result<(), ChannelError> {
        self.ensure_owner(owner, conversation).await?;
        let _management = self.binding_management_lock.lock().await;
        let lock = self
            .conversation_locks
            .entry(conversation.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;
        if let Some(expected) = expected
            && self
                .deps
                .bindings
                .get_by_conversation(owner, conversation)
                .await?
                .as_ref()
                .map(|binding| binding.id.as_str())
                != Some(expected)
        {
            return Ok(());
        }
        self.deps.bindings.delete_binding(owner, conversation).await?;
        self.interactions
            .lock()
            .await
            .retain(|_, pending| pending.binding.conversation_id != conversation);
        info!(
            owner_user_id = owner,
            conversation_id = conversation,
            "feishu: conversation unbound"
        );
        Ok(())
    }

    async fn validate_enterprise(&self, identity: &FeishuIdentity) -> Result<(), ChannelError> {
        let config = self
            .deps
            .login_config
            .get()
            .await?
            .ok_or_else(|| ChannelError::InvalidConfig("Feishu login is not configured".into()))?;
        if !config.enabled
            || config.app_id != identity.app_id
            || config.tenant_key.as_deref() != Some(identity.tenant_key.as_str())
        {
            return Err(ChannelError::UserNotAuthorized(
                "Feishu application or enterprise is not allowed".into(),
            ));
        }
        Ok(())
    }

    /// Returns true when the message belongs to the bound-conversation flow.
    /// Legacy plugins and old unannotated Lark fixtures keep the existing path.
    pub async fn handle_incoming(&self, message: &UnifiedIncomingMessage) -> bool {
        if message.platform != PluginType::Lark {
            return false;
        }
        let Some(identity) = FeishuIdentity::from_message(message) else {
            return message
                .raw
                .as_ref()
                .is_some_and(|raw| raw.get("feishu_transport").is_some());
        };
        if let Err(error) = self.handle_authenticated_message(message, &identity, None).await {
            warn!(chat_id = %message.chat_id, error = %error, "feishu: bound chat request rejected");
            if let Some(connector) = message.owner_user_id.as_deref() {
                let _ = self
                    .deps
                    .sender
                    .send_message(
                        connector,
                        "lark",
                        &message.chat_id,
                        outgoing("操作未完成，请确认会话绑定与账号权限后重试。", None),
                    )
                    .await;
            }
        }
        true
    }

    pub(super) async fn handle_authenticated_message(
        &self,
        message: &UnifiedIncomingMessage,
        identity: &FeishuIdentity,
        accepted_generation: Option<Option<&str>>,
    ) -> Result<(), ChannelError> {
        self.validate_enterprise(identity).await?;
        // A group message without a bot mention is never an implicit command.
        if !identity.callback && identity.chat_type == "group" && !identity.mentioned_bot {
            return Ok(());
        }
        let connector = message
            .owner_user_id
            .as_deref()
            .ok_or_else(|| ChannelError::InvalidConfig("Channel connector is missing".into()))?;
        if let Some(code) = message.content.text.trim().strip_prefix("/bind ").map(str::trim) {
            if identity.union_id.is_empty() {
                return Err(ChannelError::UserNotAuthorized(
                    "Feishu user identity is unavailable".into(),
                ));
            }
            // Serialize management first so the affected old/new set cannot
            // change while acquiring conversation locks in stable order.
            let _management = self.binding_management_lock.lock().await;
            let pending = self
                .deps
                .bindings
                .get_code(code)
                .await?
                .ok_or_else(|| ChannelError::UserNotAuthorized("Binding code is unavailable".into()))?;
            let previous = self
                .deps
                .bindings
                .get_by_chat(&identity.app_id, &identity.tenant_key, &message.chat_id)
                .await?;
            let mut conversations = vec![pending.conversation_id];
            if let Some(previous) = previous {
                conversations.push(previous.conversation_id);
            }
            conversations.sort();
            conversations.dedup();
            let mut guards = Vec::new();
            for conversation in conversations {
                let lock = self
                    .conversation_locks
                    .entry(conversation)
                    .or_insert_with(|| Arc::new(Mutex::new(())))
                    .clone();
                guards.push(lock.lock_owned().await);
            }
            // Redemption revalidates expiry, actual owner and both mappings
            // transactionally after all potentially conflicting operations drain.
            let row = self
                .deps
                .bindings
                .redeem_code(RedeemFeishuBindingParams {
                    code,
                    external_user_id: &format!("feishu:{}", identity.union_id),
                    connector_user_id: connector,
                    app_id: &identity.app_id,
                    tenant_key: &identity.tenant_key,
                    chat_id: &message.chat_id,
                    chat_type: &identity.chat_type,
                    now: now_ms(),
                })
                .await?;
            self.interactions.lock().await.retain(|_, pending| {
                pending.binding.conversation_id != row.conversation_id && pending.binding.chat_id != row.chat_id
            });
            info!(conversation_id = %row.conversation_id, chat_id = %row.chat_id, "feishu: conversation bound");
            drop(guards);
            drop(_management);
            self.send_text(
                &row,
                "已关联会话。此后新增的消息和回复会双向同步，已有历史不会逐条推送。",
            )
            .await?;
            return Ok(());
        }
        let Some(binding) = self
            .deps
            .bindings
            .get_by_chat(&identity.app_id, &identity.tenant_key, &message.chat_id)
            .await?
        else {
            if accepted_generation.is_some_and(|generation| generation.is_some()) {
                return Ok(());
            }
            if message.action.is_none() {
                self.deps
                    .sender
                    .send_message(
                        connector,
                        "lark",
                        &message.chat_id,
                        outgoing("请先在网页会话中选择「关联飞书」，再在此聊天发送 /bind 绑定码。", None),
                    )
                    .await?;
            }
            return Ok(());
        };
        if accepted_generation.is_some_and(|generation| generation != Some(binding.id.as_str())) {
            info!(event_id = %message.id, "feishu: stale accepted binding generation discarded");
            return Ok(());
        }
        if !identity.may_participate(&binding) {
            return Ok(());
        }
        if let Some(action) = &message.action {
            return self.handle_interaction(&binding, message, action).await;
        }
        if let Some(answer) = message.content.text.trim().strip_prefix("/answer ") {
            return self.handle_text_answer(&binding, answer).await;
        }
        if message.content.text.trim() == "/unbind" {
            let owner = self
                .deps
                .users
                .find_live_local_by_external_id(&format!("feishu:{}", identity.union_id))
                .await?;
            if owner.as_ref().map(|owner| owner.id.as_str()) != Some(binding.owner_user_id.as_str()) {
                return Err(ChannelError::UserNotAuthorized(
                    "Only the conversation owner may unbind".into(),
                ));
            }
            self.unbind_generation(&binding.owner_user_id, &binding.conversation_id, Some(&binding.id))
                .await?;
            self.send_text(&binding, "会话已解绑。").await?;
            return Ok(());
        }
        if message.content.text.trim().is_empty() {
            self.send_text(&binding, "请发送文字消息；图片或文件可在网页会话中上传。")
                .await?;
            return Ok(());
        }
        // Stable origin is persisted before the transport callback is acknowledged.
        // The conversation service also reuses it as the DB primary key, so a
        // retry after persistence cannot append another message or open a turn.
        let lock = self
            .conversation_locks
            .entry(binding.conversation_id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;
        if self
            .deps
            .bindings
            .get_by_chat(&binding.app_id, &binding.tenant_key, &binding.chat_id)
            .await?
            .as_ref()
            .map(|current| &current.id)
            != Some(&binding.id)
        {
            return Ok(());
        }
        let origin = format!("feishu:{}:{}", identity.app_id, message.id);
        let text = format!(
            "[飞书 · {}]\n{}",
            message.user.display_name,
            message.content.text.trim()
        );
        self.deps.bindings.enqueue_inbox(&origin, &binding.id, &text).await?;
        Ok(())
    }

    pub(super) async fn send_text(&self, binding: &FeishuBindingRow, text: &str) -> Result<String, ChannelError> {
        self.deps
            .sender
            .send_message(
                &binding.connector_user_id,
                "lark",
                &binding.chat_id,
                outgoing(text, None),
            )
            .await
    }
}

pub(super) fn random_token() -> Result<String, ChannelError> {
    let mut bytes = [0u8; 12];
    getrandom::getrandom(&mut bytes)
        .map_err(|_| ChannelError::InvalidConfig("Secure random source is unavailable".into()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02X}")).collect())
}

pub(super) fn outgoing(text: &str, buttons: Option<Vec<Vec<crate::types::ActionButton>>>) -> UnifiedOutgoingMessage {
    UnifiedOutgoingMessage {
        message_type: OutgoingMessageType::Text,
        text: Some(text.into()),
        parse_mode: None,
        buttons,
        keyboard: None,
        image_url: None,
        file_url: None,
        file_name: None,
        media_actions: None,
        reply_to_message_id: None,
        silent: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MessageContentType, UnifiedMessageContent, UnifiedUser};

    fn binding() -> FeishuBindingRow {
        FeishuBindingRow {
            id: "binding".into(),
            conversation_id: "conv".into(),
            owner_user_id: "owner".into(),
            connector_user_id: "connector".into(),
            app_id: "app".into(),
            tenant_key: "tenant".into(),
            chat_id: "chat".into(),
            chat_type: "group".into(),
            bound_at: 1,
        }
    }

    fn message() -> UnifiedIncomingMessage {
        UnifiedIncomingMessage {
            owner_user_id: Some("connector".into()),
            id: "message".into(),
            platform: PluginType::Lark,
            chat_id: "chat".into(),
            user: UnifiedUser {
                id: "another_member".into(),
                username: None,
                display_name: "Member".into(),
                avatar_url: None,
            },
            content: UnifiedMessageContent {
                content_type: MessageContentType::Text,
                text: "Hello".into(),
                attachments: None,
            },
            timestamp: 1,
            reply_to_message_id: None,
            action: None,
            raw: Some(
                serde_json::json!({"feishu_transport":{"app_id":"app","bot_open_id":"bot"},"feishu_event":{"app_id":"app","tenant_key":"tenant","union_id":"another_union","chat_type":"group","sender_type":"user","mentioned_ids":["bot"]}}),
            ),
        }
    }

    #[test]
    fn every_group_member_can_participate_only_when_mentioning_this_bot() {
        let mut message = message();
        let identity = FeishuIdentity::from_message(&message).unwrap();
        assert!(identity.may_participate(&binding()));
        message.raw.as_mut().unwrap()["feishu_event"]["mentioned_ids"] = serde_json::json!(["some_other_bot"]);
        assert!(
            !FeishuIdentity::from_message(&message)
                .unwrap()
                .may_participate(&binding())
        );
    }

    #[test]
    fn transport_identity_rejects_bots_and_cross_application_events() {
        let mut message = message();
        message.raw.as_mut().unwrap()["feishu_event"]["sender_type"] = serde_json::json!("app");
        assert!(FeishuIdentity::from_message(&message).is_none());
        message.raw.as_mut().unwrap()["feishu_event"]["sender_type"] = serde_json::json!("user");
        message.raw.as_mut().unwrap()["feishu_event"]["app_id"] = serde_json::json!("other_app");
        assert!(FeishuIdentity::from_message(&message).is_none());
        message.raw.as_mut().unwrap()["feishu_event"]["app_id"] = serde_json::json!("app");
        message.raw.as_mut().unwrap()["feishu_event"]["tenant_key"] = serde_json::json!("other_tenant");
        assert!(
            !FeishuIdentity::from_message(&message)
                .unwrap()
                .may_participate(&binding())
        );
    }

    #[test]
    fn binding_tokens_are_unpredictable_and_not_reused() {
        let one = random_token().unwrap();
        let two = random_token().unwrap();
        assert_eq!(one.len(), 24);
        assert!(one.chars().all(|character| character.is_ascii_hexdigit()));
        assert_ne!(one, two);
    }
}
