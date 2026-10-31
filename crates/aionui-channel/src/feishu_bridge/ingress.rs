#[cfg(feature = "lark")]
use super::service::FeishuIdentity;
use super::state::FeishuBridge;
use crate::error::ChannelError;
use crate::plugin::{DurableChannelIngress, DurableIngressEvent};

/// The first committed receipt fixes which conversation may consume the event.
/// None means it was accepted while unbound; retries must never adopt a later
/// binding. INSERT OR IGNORE preserves this snapshot on transport redelivery.
#[derive(serde::Serialize, serde::Deserialize)]
struct AcceptedIngress {
    event: DurableIngressEvent,
    binding_generation: Option<String>,
}

#[async_trait::async_trait]
impl DurableChannelIngress for FeishuBridge {
    async fn accept(&self, event: DurableIngressEvent) -> Result<(), ChannelError> {
        if event.owner_user_id.is_empty() || event.app_id.is_empty() {
            return Err(ChannelError::InvalidConfig("Missing trusted connector identity".into()));
        }
        let _management = self.binding_management_lock.lock().await;
        #[cfg(feature = "lark")]
        let binding_generation = {
            let decoded = crate::plugins::lark::decode_durable_ingress(&event).await;
            if let Some(message) = decoded.first()
                && let Some(identity) = FeishuIdentity::from_message(message)
            {
                self.deps
                    .bindings
                    .get_by_chat(&identity.app_id, &identity.tenant_key, &message.chat_id)
                    .await?
                    .map(|binding| binding.id)
            } else {
                None
            }
        };
        #[cfg(not(feature = "lark"))]
        let binding_generation: Option<String> = None;
        let id = event.id.clone();
        let accepted = AcceptedIngress {
            event,
            binding_generation,
        };
        self.deps
            .bindings
            .enqueue_ingress(&id, &serde_json::to_string(&accepted)?)
            .await?;
        Ok(())
    }
}

impl FeishuBridge {
    #[cfg(feature = "lark")]
    pub(super) async fn flush_ingress(&self) -> Result<(), ChannelError> {
        for row in self.deps.bindings.pending_ingress().await? {
            let accepted: AcceptedIngress = match serde_json::from_str(&row.payload) {
                Ok(accepted) => accepted,
                Err(error) => {
                    // Older unscoped receipts cannot be safely reconstructed
                    // after a binding change; preserve the audit row, consume
                    // it without replaying into an unrelated conversation.
                    tracing::warn!(event_id = %row.id, error = %error, "feishu: unscoped receipt discarded without replay");
                    self.deps.bindings.complete_ingress(&row.id).await?;
                    continue;
                }
            };
            let mut retry = false;
            for message in crate::plugins::lark::decode_durable_ingress(&accepted.event).await {
                let Some(identity) = FeishuIdentity::from_message(&message) else {
                    continue;
                };
                if let Err(error) = self
                    .handle_authenticated_message(&message, &identity, Some(accepted.binding_generation.as_deref()))
                    .await
                {
                    match error {
                        ChannelError::Database(aionui_db::DbError::Query(_))
                        | ChannelError::MessageSendFailed(_)
                        | ChannelError::ConnectionFailed(_) => {
                            tracing::warn!(event_id = %row.id, error = %error, "feishu: accepted event retained for retry");
                            retry = true;
                        }
                        _ => {
                            tracing::warn!(event_id = %row.id, error = %error, "feishu: accepted event rejected by authorization or validation")
                        }
                    }
                }
            }
            if !retry {
                self.deps.bindings.complete_ingress(&row.id).await?;
            }
        }
        Ok(())
    }
}
