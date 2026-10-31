use super::service::ConversationShareService;

#[derive(Clone)]
pub struct ShareRouterState {
    pub service: ConversationShareService,
}
