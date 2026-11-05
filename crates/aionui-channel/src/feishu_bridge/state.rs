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
    #[cfg(feature = "lark")]
    pub gateway: Option<Arc<crate::gateway::GatewayClient>>,
    #[cfg(feature = "lark")]
    pub gateway_repository: Option<Arc<dyn aionui_db::IFeishuGatewayRepository>>,
}

/// Existing conversation transport for owner-managed Feishu chat bindings.
pub struct FeishuBridge {
    pub(super) deps: FeishuBridgeDependencies,
    pub(super) interactions: Mutex<HashMap<String, PendingInteraction>>,
    pub(super) delivery_lock: Mutex<()>,
    pub(super) binding_management_lock: Mutex<()>,
    pub(super) process_epoch: String,
    pub(super) conversation_locks: DashMap<String, Arc<Mutex<()>>>,
    #[cfg(feature = "lark")]
    pub(super) gateway_restored: Mutex<bool>,
    #[cfg(feature = "lark")]
    pub(super) gateway_attempts: Mutex<HashMap<String, String>>,
    #[cfg(feature = "lark")]
    pub(super) gateway_auth_rejected: std::sync::atomic::AtomicBool,
    #[cfg(feature = "lark")]
    pub(super) gateway_identity_checked: std::sync::atomic::AtomicBool,
    #[cfg(feature = "lark")]
    pub(super) gateway_reconcile_cursor: Mutex<usize>,
    #[cfg(feature = "lark")]
    pub(super) gateway_settled_routes: Mutex<std::collections::HashSet<String>>,
}
