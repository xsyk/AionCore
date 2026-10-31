use sqlx::{QueryBuilder, Sqlite, SqlitePool};

use super::conversation_share::IConversationShareRepository;
use crate::{
    DbError, MessagePageCursor, MessagePageDirection, MessagePageParams, MessagePageResult,
    models::{ConversationRow, ConversationShareRow, MessageRow},
};

const VISIBLE_MESSAGES: &str = " FROM messages m INNER JOIN conversation_shares s ON s.conversation_id = m.conversation_id INNER JOIN conversations c ON c.id = s.conversation_id WHERE s.token = ? AND s.enabled = 1 AND m.hidden = 0 AND m.type NOT IN ('cron_trigger', 'skill_suggest', 'available_commands')";

#[derive(Clone, Debug)]
pub struct SqliteConversationShareRepository {
    pool: SqlitePool,
}

impl SqliteConversationShareRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    async fn ensure_owner(&self, owner_id: &str, conversation_id: &str) -> Result<(), DbError> {
        let owned: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM conversations WHERE id = ? AND user_id = ?)")
            .bind(conversation_id)
            .bind(owner_id)
            .fetch_one(&self.pool)
            .await?;
        if owned {
            Ok(())
        } else {
            Err(DbError::NotFound("Conversation not found".into()))
        }
    }

    async fn exists_beyond(&self, token: &str, cursor: &MessagePageCursor, before: bool) -> Result<bool, DbError> {
        let operator = if before { "<" } else { ">" };
        let sql = format!(
            "SELECT EXISTS(SELECT 1{VISIBLE_MESSAGES} AND (m.created_at {operator} ? OR (m.created_at = ? AND m.id {operator} ?)))"
        );
        Ok(sqlx::query_scalar(&sql)
            .bind(token)
            .bind(cursor.created_at)
            .bind(cursor.created_at)
            .bind(&cursor.id)
            .fetch_one(&self.pool)
            .await?)
    }
}

#[async_trait::async_trait]
impl IConversationShareRepository for SqliteConversationShareRepository {
    async fn get_for_owner(
        &self,
        owner_id: &str,
        conversation_id: &str,
    ) -> Result<Option<ConversationShareRow>, DbError> {
        self.ensure_owner(owner_id, conversation_id).await?;
        Ok(sqlx::query_as("SELECT s.* FROM conversation_shares s INNER JOIN conversations c ON c.id = s.conversation_id WHERE s.conversation_id = ? AND c.user_id = ?")
            .bind(conversation_id).bind(owner_id).fetch_optional(&self.pool).await?)
    }

    async fn set_enabled(
        &self,
        owner_id: &str,
        conversation_id: &str,
        new_token: &str,
        enabled: bool,
    ) -> Result<ConversationShareRow, DbError> {
        let now = aionui_common::now_ms();
        // Ownership and the write are checked atomically. The old token is
        // deliberately absent from the conflict-update clause.
        sqlx::query_as("INSERT INTO conversation_shares (conversation_id,token,enabled,created_at,updated_at) SELECT id,?,?,?,? FROM conversations WHERE id = ? AND user_id = ? ON CONFLICT(conversation_id) DO UPDATE SET enabled = excluded.enabled, updated_at = excluded.updated_at RETURNING *")
            .bind(new_token).bind(enabled).bind(now).bind(now).bind(conversation_id).bind(owner_id)
            .fetch_optional(&self.pool).await?.ok_or_else(|| DbError::NotFound("Conversation not found".into()))
    }

    async fn find_public(&self, token: &str) -> Result<Option<ConversationRow>, DbError> {
        Ok(sqlx::query_as("SELECT c.* FROM conversations c INNER JOIN conversation_shares s ON s.conversation_id = c.id WHERE s.token = ? AND s.enabled = 1")
            .bind(token).fetch_optional(&self.pool).await?)
    }

    async fn list_public_messages(
        &self,
        token: &str,
        params: &MessagePageParams,
    ) -> Result<MessagePageResult, DbError> {
        self.find_public(token)
            .await?
            .ok_or_else(|| DbError::NotFound("Shared conversation is unavailable".into()))?;
        let mut query = QueryBuilder::<Sqlite>::new("SELECT m.*");
        // Keep the token at the capability predicate rather than appending
        // it after the visibility predicates.
        let (prefix, suffix) = VISIBLE_MESSAGES.split_once('?').expect("token placeholder");
        query.push(prefix).push_bind(token).push(suffix);
        let ascending = matches!(&params.direction, MessagePageDirection::After { .. });
        match &params.direction {
            MessagePageDirection::InitialLatest => {}
            MessagePageDirection::Before { cursor } | MessagePageDirection::After { cursor } => {
                let operator = if ascending { ">" } else { "<" };
                query
                    .push(format!(" AND (m.created_at {operator} "))
                    .push_bind(cursor.created_at)
                    .push(" OR (m.created_at = ")
                    .push_bind(cursor.created_at)
                    .push(format!(" AND m.id {operator} "))
                    .push_bind(&cursor.id)
                    .push("))");
            }
            MessagePageDirection::Anchor { .. } => {
                return Err(DbError::Init("Public share pages support before/after cursors".into()));
            }
        }
        if ascending {
            query.push(" ORDER BY m.created_at ASC, m.id ASC");
        } else {
            query.push(" ORDER BY m.created_at DESC, m.id DESC");
        }
        query.push(" LIMIT ").push_bind(i64::from(params.limit.clamp(1, 200)));
        let mut items = query.build_query_as::<MessageRow>().fetch_all(&self.pool).await?;
        if !ascending {
            items.reverse();
        }
        let (has_more_before, has_more_after) = if let (Some(first), Some(last)) = (items.first(), items.last()) {
            (
                self.exists_beyond(token, &first.into(), true).await?,
                self.exists_beyond(token, &last.into(), false).await?,
            )
        } else {
            (false, false)
        };
        Ok(MessagePageResult {
            items,
            has_more_before,
            has_more_after,
        })
    }

    async fn list_public_artifacts(&self, token: &str) -> Result<Vec<crate::models::ConversationArtifactRow>, DbError> {
        self.find_public(token)
            .await?
            .ok_or_else(|| DbError::NotFound("Shared conversation is unavailable".into()))?;
        Ok(sqlx::query_as("SELECT a.* FROM conversation_artifacts a INNER JOIN conversation_shares s ON s.conversation_id = a.conversation_id INNER JOIN conversations c ON c.id = s.conversation_id WHERE s.token = ? AND s.enabled = 1 ORDER BY a.created_at ASC, a.id ASC")
            .bind(token).fetch_all(&self.pool).await?)
    }

    async fn list_public_legacy_triggers(&self, token: &str) -> Result<Vec<MessageRow>, DbError> {
        self.find_public(token)
            .await?
            .ok_or_else(|| DbError::NotFound("Shared conversation is unavailable".into()))?;
        Ok(sqlx::query_as("SELECT m.* FROM messages m INNER JOIN conversation_shares s ON s.conversation_id = m.conversation_id INNER JOIN conversations c ON c.id = s.conversation_id WHERE s.token = ? AND s.enabled = 1 AND m.hidden = 0 AND m.type = 'cron_trigger' ORDER BY m.created_at ASC, m.id ASC")
            .bind(token).fetch_all(&self.pool).await?)
    }
}
