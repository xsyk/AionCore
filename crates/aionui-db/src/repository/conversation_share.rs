use crate::{
    DbError, MessagePageParams, MessagePageResult,
    models::{ConversationRow, ConversationShareRow},
};

/// Owner-scoped management and capability-scoped, side-effect-free public reads.
#[async_trait::async_trait]
pub trait IConversationShareRepository: Send + Sync {
    /// Missing/foreign conversations are rejected even when no share exists yet.
    async fn get_for_owner(
        &self,
        owner_id: &str,
        conversation_id: &str,
    ) -> Result<Option<ConversationShareRow>, DbError>;

    /// Only the initial insert uses `new_token`; toggling never replaces it.
    async fn set_enabled(
        &self,
        owner_id: &str,
        conversation_id: &str,
        new_token: &str,
        enabled: bool,
    ) -> Result<ConversationShareRow, DbError>;

    /// Joins the live parent so deleted conversations cannot remain accessible.
    async fn find_public(&self, token: &str) -> Result<Option<ConversationRow>, DbError>;

    /// Filters hidden/internal rows in SQL, before cursor pagination.
    async fn list_public_messages(&self, token: &str, params: &MessagePageParams)
    -> Result<MessagePageResult, DbError>;

    async fn list_public_artifacts(&self, token: &str) -> Result<Vec<crate::models::ConversationArtifactRow>, DbError>;

    async fn list_public_legacy_triggers(&self, token: &str) -> Result<Vec<crate::models::MessageRow>, DbError>;
}
