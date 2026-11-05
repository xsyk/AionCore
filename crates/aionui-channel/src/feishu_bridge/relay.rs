use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use aionui_ai_agent::protocol::events::AcpPermissionEventData;
use aionui_api_types::{SendMessageRequest, WebSocketMessage};
use aionui_common::{Confirmation, now_ms};
use aionui_db::models::FeishuOutboxRow;
use serde_json::Value;
use tokio::sync::broadcast;
use tracing::warn;

use super::service::outgoing;
use super::state::FeishuBridge;
use crate::error::ChannelError;

impl FeishuBridge {
    /// Event consumption and remote I/O are separate futures. Slow Feishu calls
    /// never block the event receiver. Text recovery uses durable DB rows, so
    /// broadcast lag and process restarts cannot lose accepted conversation text.
    pub async fn run(self: Arc<Self>, mut events: broadcast::Receiver<WebSocketMessage<Value>>) {
        let consume = async {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        if let Err(error) = self.handle_event(event).await {
                            warn!(error = %error, "feishu: event handling failed");
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => warn!(
                        skipped,
                        "feishu: event subscription lagged; text will recover from persistence"
                    ),
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        };
        let deliver = async {
            let mut tick = tokio::time::interval(Duration::from_millis(750));
            loop {
                tick.tick().await;
                if let Err(error) = self.flush_deliveries().await {
                    warn!(error = %error, "feishu: pending delivery retained for retry");
                }
                self.flush_interactions().await;
                if self.expire_interactions().await.is_err() {
                    warn!(
                        code = "FEISHU_INTERACTION_CLEANUP_FAILED",
                        "feishu: expired interaction cleanup will retry"
                    );
                }
            }
        };
        // Dropping either future on shutdown also cancels its in-flight request;
        // a future restart retries the durable queue using the same request ID.
        #[cfg(feature = "lark")]
        {
            let gateway = async {
                if self.deps.gateway.is_none() {
                    std::future::pending::<()>().await;
                }
                let mut tick = tokio::time::interval(Duration::from_millis(750));
                loop {
                    tick.tick().await;
                    if self.flush_gateway().await.is_err() {
                        warn!(
                            code = "GATEWAY_SYNC_FAILED",
                            "feishu: gateway synchronization will retry"
                        );
                    }
                }
            };
            tokio::select! { _ = consume => {}, _ = deliver => {}, _ = gateway => {} }
        }
        #[cfg(not(feature = "lark"))]
        tokio::select! { _ = consume => {}, _ = deliver => {} }
    }

    pub async fn handle_event(&self, event: WebSocketMessage<Value>) -> Result<(), ChannelError> {
        #[cfg(feature = "lark")]
        self.ensure_gateway_identity().await?;
        if !matches!(
            event.name.as_str(),
            "message.stream" | "turn.completed" | "confirmation.remove"
        ) {
            return Ok(());
        }
        if event.name == "message.stream"
            && !matches!(
                event.data.get("type").and_then(Value::as_str),
                Some("ask" | "permission" | "acp_permission")
            )
        {
            return Ok(());
        }
        let Some(conversation) = event.data.get("conversation_id").and_then(Value::as_str) else {
            return Ok(());
        };
        let Some(owner) = event.data.get("user_id").and_then(Value::as_str) else {
            return Ok(());
        };
        let Some(binding) = self.deps.bindings.get_by_conversation(owner, conversation).await? else {
            return Ok(());
        };
        #[cfg(feature = "lark")]
        if !self.gateway_binding_active(&binding.id).await? {
            return Ok(());
        }
        if event.name == "confirmation.remove" {
            let id = event.data.get("id").and_then(Value::as_str).unwrap_or_default();
            self.discard_conversation_interactions(conversation, Some(id)).await?;
        } else if event.name == "turn.completed" {
            self.discard_conversation_interactions(conversation, None).await?;
        } else {
            let msg_id = event.data.get("msg_id").and_then(Value::as_str).unwrap_or_default();
            let data = &event.data["data"];
            match event.data.get("type").and_then(Value::as_str).unwrap_or_default() {
                "ask" => self.publish_question(&binding, msg_id, data).await?,
                "permission" | "acp_permission" => {
                    let confirmation = serde_json::from_value::<AcpPermissionEventData>(data.clone())
                        .ok()
                        .and_then(|data| data.as_confirmation())
                        .or_else(|| serde_json::from_value::<Confirmation>(data.clone()).ok());
                    if let Some(confirmation) = confirmation {
                        self.publish_confirmation(&binding, msg_id, confirmation).await?;
                    } else {
                        let confirmations = self
                            .deps
                            .conversations
                            .list_confirmations(owner, conversation, &self.deps.tasks)
                            .await
                            .map_err(|error| ChannelError::MessageSendFailed(error.to_string()))?;
                        for confirmation in confirmations {
                            self.publish_confirmation(&binding, msg_id, confirmation).await?;
                        }
                    }
                }
                // In particular, neither finish nor replace/hidden is a final
                // text boundary: conversation middleware can rewrite afterwards.
                _ => {}
            }
        }
        Ok(())
    }

    pub(super) async fn flush_deliveries(&self) -> Result<(), ChannelError> {
        #[cfg(feature = "lark")]
        self.ensure_gateway_identity().await?;
        let _delivery = self.delivery_lock.lock().await;
        #[cfg(feature = "lark")]
        if self.deps.gateway.is_none() {
            self.flush_ingress().await?;
        }
        self.flush_inbox().await?;
        let bindings = self.deps.bindings.list_bindings().await?;
        for binding in &bindings {
            #[cfg(feature = "lark")]
            if !self.gateway_binding_active(&binding.id).await? {
                continue;
            }
            let before = self
                .deps
                .conversations
                .runtime_summary_for(&binding.conversation_id)
                .await;
            let candidates = self.deps.bindings.sync_candidates(&binding.id).await?;
            let after = self
                .deps
                .conversations
                .runtime_summary_for(&binding.conversation_id)
                .await;
            for message in candidates {
                let is_user = message.position.as_deref() == Some("right");
                if !is_user && (before.turn_id.is_some() || after.turn_id.is_some()) {
                    continue;
                }
                let value: Value = serde_json::from_str(&message.content).unwrap_or(Value::Null);
                let text = value.get("content").and_then(Value::as_str).unwrap_or_default();
                let text = if is_user {
                    format!("网页消息\n{text}")
                } else {
                    text.to_owned()
                };
                let parts = if text.is_empty() {
                    vec![String::new()]
                } else {
                    split_text(&text, 3200)
                };
                let rows: Vec<_> = parts
                    .into_iter()
                    .enumerate()
                    .map(|(index, content)| FeishuOutboxRow {
                        id: aionui_common::generate_id(),
                        binding_id: binding.id.clone(),
                        message_id: message.id.clone(),
                        part: index as i64,
                        content,
                        platform_message_id: None,
                        first_attempt_at: None,
                    })
                    .collect();
                self.deps.bindings.enqueue_outbox(&rows).await?;
            }
        }
        #[cfg(feature = "lark")]
        let park_all = self.deps.gateway.is_none();
        #[cfg(not(feature = "lark"))]
        let park_all = true;
        let parked = if park_all {
            self.deps
                .bindings
                .park_expired_outbox(now_ms() - 55 * 60 * 1000)
                .await?
        } else {
            0
        };
        if parked > 0 {
            warn!(
                parked,
                "feishu: unknown deliveries parked for audit outside automatic queue"
            );
        }
        let mut blocked_bindings = HashSet::new();
        for row in self.deps.bindings.pending_outbox().await? {
            if blocked_bindings.contains(&row.binding_id) {
                continue;
            }
            let Some(binding) = bindings.iter().find(|binding| binding.id == row.binding_id) else {
                continue;
            };
            #[cfg(feature = "lark")]
            if !self.gateway_binding_active(&binding.id).await? {
                continue;
            }
            // The legacy provider UUID expires after one hour. Gateway retries
            // use its durable key and provider attempt time: a local attempt may
            // have failed before the gateway received anything.
            if park_all
                && row
                    .first_attempt_at
                    .is_some_and(|time| now_ms().saturating_sub(time) >= 55 * 60 * 1000)
            {
                warn!(delivery_id = %row.id, conversation_id = %binding.conversation_id, "feishu: delivery outcome unknown beyond idempotency window; retained for inspection");
                self.deps.bindings.park_outbox(&row.id).await?;
                continue;
            }
            if row.content.is_empty() {
                self.deps.bindings.complete_outbox(&row.id, "empty").await?;
                continue;
            }
            let lock = self
                .conversation_locks
                .entry(binding.conversation_id.clone())
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
                continue;
            }
            self.deps.bindings.start_outbox_attempt(&row.id, now_ms()).await?;
            match self
                .send_bound_message(binding, outgoing(&row.content, None), Some(&row.id))
                .await
            {
                Ok(id) => self.deps.bindings.complete_outbox(&row.id, &id).await?,
                Err(error) => {
                    warn!(delivery_id = %row.id, error = %error, "feishu: delivery failed; queued for retry");
                    blocked_bindings.insert(binding.id.clone());
                }
            }
        }
        Ok(())
    }

    pub(super) async fn flush_inbox(&self) -> Result<(), ChannelError> {
        #[cfg(feature = "lark")]
        self.ensure_gateway_identity().await?;
        let bindings = self.deps.bindings.list_bindings().await?;
        for row in self.deps.bindings.pending_inbox().await? {
            let Some(binding) = bindings.iter().find(|binding| binding.id == row.binding_id) else {
                continue;
            };
            #[cfg(feature = "lark")]
            if !self.gateway_binding_active(&binding.id).await? {
                continue;
            }
            let lock = self
                .conversation_locks
                .entry(binding.conversation_id.clone())
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
                continue;
            }
            let stored = self
                .deps
                .conversation_repo
                .get_message(&binding.owner_user_id, &binding.conversation_id, &row.id)
                .await?;
            if stored.is_some() && row.attempt_epoch.as_deref() != Some(self.process_epoch.as_str()) {
                // Startup recovery settles old working assistant/tool rows, but
                // cannot prove an already-finished user row reached the agent.
                // Preserve it and surface uncertainty rather than execute again.
                let tip_id = format!("{}:interrupted", row.id);
                if self
                    .deps
                    .conversation_repo
                    .get_message(&binding.owner_user_id, &binding.conversation_id, &tip_id)
                    .await?
                    .is_none()
                {
                    self.deps.conversations.insert_raw_message(&binding.owner_user_id, &aionui_db::models::MessageRow {
                        id: tip_id.clone(), conversation_id: binding.conversation_id.clone(), msg_id: Some(tip_id), r#type: "tips".into(),
                        content: serde_json::json!({"content":"服务重启前已保存这条飞书消息，但无法确认是否交给 AI 执行。请先核对会话结果；需要时重新发送。","type":"warning"}).to_string(),
                        position: Some("left".into()), status: Some("finish".into()), hidden: false, created_at: now_ms(), backend_turn_id: None,
                    }).await.map_err(|error| ChannelError::MessageSendFailed(error.to_string()))?;
                }
                self.deps.bindings.complete_inbox(&row.id).await?;
                continue;
            }
            #[cfg(feature = "lark")]
            self.ensure_gateway_identity().await?;
            self.deps
                .bindings
                .start_inbox_attempt(&row.id, &self.process_epoch)
                .await?;
            match self
                .deps
                .conversations
                .send_transport_message(
                    &binding.owner_user_id,
                    &binding.conversation_id,
                    SendMessageRequest {
                        content: row.content,
                        files: vec![],
                        sessions: vec![],
                        inject_skills: vec![],
                        hidden: false,
                    },
                    &self.deps.tasks,
                    &row.id,
                )
                .await
            {
                Ok(_) => self.deps.bindings.complete_inbox(&row.id).await?,
                Err(error) => {
                    warn!(delivery_id = %row.id, error = %error, "feishu: input not yet accepted; queued for retry")
                }
            }
        }
        Ok(())
    }
}

pub(super) fn split_text(text: &str, maximum: usize) -> Vec<String> {
    let mut result = Vec::new();
    let mut part = String::new();
    let mut count = 0;
    for character in text.chars() {
        if count == maximum {
            result.push(part);
            part = String::new();
            count = 0;
        }
        part.push(character);
        count += 1;
    }
    if !part.is_empty() {
        result.push(part);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::split_text;
    #[test]
    fn long_unicode_replies_are_split_without_truncation() {
        let text = "你好🌍".repeat(1600);
        let parts = split_text(&text, 3200);
        assert!(parts.len() > 1);
        assert!(parts.iter().all(|part| part.chars().count() <= 3200));
        assert_eq!(parts.concat(), text);
    }
}
