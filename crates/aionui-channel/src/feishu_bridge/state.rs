use std::collections::HashMap;
use std::sync::Arc;

use aionui_ai_agent::IWorkerTaskManager;
use aionui_conversation::ConversationService;
use aionui_db::{IConversationRepository, IFeishuBindingRepository, IFeishuLoginRepository, IUserRepository};
use dashmap::DashMap;
use tokio::sync::Mutex;

use super::interactions::PendingInteraction;
use crate::stream_relay::ChannelSender;

/// Constructed in the application composition layer.
pub struct FeishuBridgeDependencies {
    pub bindings: Arc<dyn IFeishuBindingRepository>,
    pub conversations: Arc<ConversationService>,
    pub conversation_repo: Arc<dyn IConversationRepository>,
    pub users: Arc<dyn IUserRepository>,
    pub login_config: Arc<dyn IFeishuLoginRepository>,
    pub tasks: Arc<dyn IWorkerTaskManager>,
    pub sender: Arc<dyn ChannelSender>,
}

/// Existing conversation transport for owner-managed Feishu chat bindings.
pub struct FeishuBridge {
    pub(super) deps: FeishuBridgeDependencies,
    pub(super) interactions: Mutex<HashMap<String, PendingInteraction>>,
    pub(super) delivery_lock: Mutex<()>,
    pub(super) binding_management_lock: Mutex<()>,
    pub(super) process_epoch: String,
    pub(super) conversation_locks: DashMap<String, Arc<Mutex<()>>>,
}
