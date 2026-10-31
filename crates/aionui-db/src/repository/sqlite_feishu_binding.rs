use crate::models::{
    FeishuBindingCodeRow, FeishuBindingRow, FeishuInboxRow, FeishuIngressRow, FeishuOutboxRow, MessageRow,
    RedeemFeishuBindingParams,
};
use crate::{DbError, IFeishuBindingRepository, SqlitePool};

pub struct SqliteFeishuBindingRepository {
    pool: SqlitePool,
}

impl SqliteFeishuBindingRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IFeishuBindingRepository for SqliteFeishuBindingRepository {
    async fn get_by_conversation(&self, owner: &str, conversation: &str) -> Result<Option<FeishuBindingRow>, DbError> {
        Ok(sqlx::query_as("SELECT b.* FROM feishu_conversation_bindings b JOIN conversations c ON c.id = b.conversation_id JOIN users u ON u.id = b.owner_user_id WHERE b.owner_user_id = ? AND b.conversation_id = ? AND c.user_id = b.owner_user_id AND u.status = 'active' AND u.deleted_at IS NULL")
            .bind(owner).bind(conversation).fetch_optional(&self.pool).await?)
    }

    async fn get_by_chat(&self, app: &str, tenant: &str, chat: &str) -> Result<Option<FeishuBindingRow>, DbError> {
        Ok(sqlx::query_as("SELECT b.* FROM feishu_conversation_bindings b JOIN conversations c ON c.id = b.conversation_id JOIN users u ON u.id = b.owner_user_id WHERE b.app_id = ? AND b.tenant_key = ? AND b.chat_id = ? AND c.user_id = b.owner_user_id AND u.status = 'active' AND u.deleted_at IS NULL")
            .bind(app).bind(tenant).bind(chat).fetch_optional(&self.pool).await?)
    }

    async fn save_code(&self, code: &FeishuBindingCodeRow) -> Result<(), DbError> {
        let changed = sqlx::query("INSERT INTO feishu_conversation_binding_codes (code, conversation_id, owner_user_id, expires_at) SELECT ?, id, user_id, ? FROM conversations WHERE id = ? AND user_id = ? ON CONFLICT(conversation_id) DO UPDATE SET code = excluded.code, owner_user_id = excluded.owner_user_id, expires_at = excluded.expires_at")
            .bind(&code.code).bind(code.expires_at).bind(&code.conversation_id).bind(&code.owner_user_id).execute(&self.pool).await?;
        if changed.rows_affected() == 0 {
            return Err(DbError::NotFound("Conversation not found".into()));
        }
        Ok(())
    }

    async fn redeem_code(&self, p: RedeemFeishuBindingParams<'_>) -> Result<FeishuBindingRow, DbError> {
        if !matches!(p.chat_type, "p2p" | "group")
            || p.chat_id.is_empty()
            || p.app_id.is_empty()
            || p.tenant_key.is_empty()
        {
            return Err(DbError::Conflict("Invalid Feishu chat identity".into()));
        }
        let mut tx = self.pool.begin().await?;
        // Acquire the write lock before reading the code, so redemption is atomic
        // across independent connections and server instances.
        sqlx::query("DELETE FROM feishu_conversation_binding_codes WHERE expires_at <= ?")
            .bind(p.now)
            .execute(&mut *tx)
            .await?;
        let code: FeishuBindingCodeRow = sqlx::query_as("SELECT b.* FROM feishu_conversation_binding_codes b JOIN conversations c ON c.id = b.conversation_id AND c.user_id = b.owner_user_id JOIN users u ON u.id = b.owner_user_id WHERE b.code = ? AND b.expires_at > ? AND u.external_user_id = ? AND u.status = 'active' AND u.deleted_at IS NULL")
            .bind(p.code).bind(p.now).bind(p.external_user_id).fetch_optional(&mut *tx).await?
            .ok_or_else(|| DbError::NotFound("Binding code is invalid, expired, or belongs to another user".into()))?;
        let existing_owner: Option<String> = sqlx::query_scalar("SELECT owner_user_id FROM feishu_conversation_bindings WHERE app_id = ? AND tenant_key = ? AND chat_id = ?")
            .bind(p.app_id).bind(p.tenant_key).bind(p.chat_id).fetch_optional(&mut *tx).await?;
        if existing_owner
            .as_deref()
            .is_some_and(|owner| owner != code.owner_user_id)
        {
            return Err(DbError::Conflict(
                "Only the current conversation owner may change this chat binding".into(),
            ));
        }
        sqlx::query("DELETE FROM feishu_conversation_bindings WHERE conversation_id = ? OR (app_id = ? AND tenant_key = ? AND chat_id = ?)")
            .bind(&code.conversation_id).bind(p.app_id).bind(p.tenant_key).bind(p.chat_id).execute(&mut *tx).await?;
        let row = FeishuBindingRow {
            id: aionui_common::generate_id(),
            conversation_id: code.conversation_id,
            owner_user_id: code.owner_user_id,
            connector_user_id: p.connector_user_id.into(),
            app_id: p.app_id.into(),
            tenant_key: p.tenant_key.into(),
            chat_id: p.chat_id.into(),
            chat_type: p.chat_type.into(),
            bound_at: p.now,
        };
        sqlx::query("INSERT INTO feishu_conversation_bindings (id, conversation_id, owner_user_id, connector_user_id, app_id, tenant_key, chat_id, chat_type, bound_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(&row.id).bind(&row.conversation_id).bind(&row.owner_user_id).bind(&row.connector_user_id).bind(&row.app_id).bind(&row.tenant_key).bind(&row.chat_id).bind(&row.chat_type).bind(row.bound_at).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO feishu_binding_baseline_messages(binding_id,message_id) SELECT ?,id FROM messages WHERE conversation_id=?").bind(&row.id).bind(&row.conversation_id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM feishu_conversation_binding_codes WHERE code = ?")
            .bind(p.code)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(row)
    }

    async fn delete_binding(&self, owner: &str, conversation: &str) -> Result<bool, DbError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM feishu_conversation_binding_codes WHERE owner_user_id = ? AND conversation_id = ?")
            .bind(owner)
            .bind(conversation)
            .execute(&mut *tx)
            .await?;
        let changed =
            sqlx::query("DELETE FROM feishu_conversation_bindings WHERE owner_user_id = ? AND conversation_id = ?")
                .bind(owner)
                .bind(conversation)
                .execute(&mut *tx)
                .await?;
        tx.commit().await?;
        Ok(changed.rows_affected() > 0)
    }

    async fn list_bindings(&self) -> Result<Vec<FeishuBindingRow>, DbError> {
        Ok(sqlx::query_as("SELECT b.* FROM feishu_conversation_bindings b JOIN conversations c ON c.id=b.conversation_id AND c.user_id=b.owner_user_id JOIN users u ON u.id=b.owner_user_id WHERE u.status='active' AND u.deleted_at IS NULL").fetch_all(&self.pool).await?)
    }
    async fn enqueue_inbox(&self, id: &str, binding: &str, content: &str) -> Result<(), DbError> {
        sqlx::query("INSERT OR IGNORE INTO feishu_inbox(id,binding_id,content) VALUES(?,?,?)")
            .bind(id)
            .bind(binding)
            .bind(content)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    async fn pending_inbox(&self) -> Result<Vec<FeishuInboxRow>, DbError> {
        Ok(sqlx::query_as(
            "SELECT id,binding_id,content,attempt_epoch FROM feishu_inbox WHERE done=0 ORDER BY rowid LIMIT 100",
        )
        .fetch_all(&self.pool)
        .await?)
    }
    async fn complete_inbox(&self, id: &str) -> Result<(), DbError> {
        sqlx::query("UPDATE feishu_inbox SET done=1 WHERE id=?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    async fn sync_candidates(&self, binding: &str) -> Result<Vec<MessageRow>, DbError> {
        Ok(sqlx::query_as("SELECT m.* FROM messages m JOIN feishu_conversation_bindings b ON b.conversation_id=m.conversation_id WHERE b.id=? AND NOT EXISTS(SELECT 1 FROM feishu_binding_baseline_messages h WHERE h.binding_id=b.id AND h.message_id=m.id) AND m.hidden=0 AND m.type IN ('text','tips') AND (COALESCE(m.position,'left')<>'right' OR m.id NOT LIKE 'feishu:%') AND (m.position='right' OR m.status IN ('finish','error')) AND (m.position='right' OR m.type='tips' OR CASE WHEN json_valid(m.content) THEN json_extract(m.content,'$.__aion_finalized')=1 ELSE 0 END) AND NOT EXISTS(SELECT 1 FROM feishu_outbox o WHERE o.binding_id=b.id AND o.message_id=m.id) ORDER BY m.created_at,m.id LIMIT 100").bind(binding).fetch_all(&self.pool).await?)
    }
    async fn enqueue_outbox(&self, rows: &[FeishuOutboxRow]) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        for row in rows {
            sqlx::query("INSERT OR IGNORE INTO feishu_outbox(id,binding_id,message_id,part,content) VALUES(?,?,?,?,?)")
                .bind(&row.id)
                .bind(&row.binding_id)
                .bind(&row.message_id)
                .bind(row.part)
                .bind(&row.content)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
    async fn pending_outbox(&self) -> Result<Vec<FeishuOutboxRow>, DbError> {
        Ok(sqlx::query_as("SELECT * FROM feishu_outbox WHERE platform_message_id IS NULL AND delivery_state='pending' ORDER BY rowid LIMIT 100").fetch_all(&self.pool).await?)
    }
    async fn start_outbox_attempt(&self, id: &str, now: i64) -> Result<(), DbError> {
        sqlx::query("UPDATE feishu_outbox SET first_attempt_at=COALESCE(first_attempt_at,?) WHERE id=?")
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    async fn complete_outbox(&self, id: &str, platform_id: &str) -> Result<(), DbError> {
        sqlx::query("UPDATE feishu_outbox SET platform_message_id=? WHERE id=?")
            .bind(platform_id)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    async fn get_code(&self, code: &str) -> Result<Option<FeishuBindingCodeRow>, DbError> {
        Ok(
            sqlx::query_as("SELECT * FROM feishu_conversation_binding_codes WHERE code=?")
                .bind(code)
                .fetch_optional(&self.pool)
                .await?,
        )
    }
    async fn enqueue_ingress(&self, id: &str, payload: &str) -> Result<(), DbError> {
        sqlx::query("INSERT OR IGNORE INTO feishu_ingress_events(id,payload) VALUES(?,?)")
            .bind(id)
            .bind(payload)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    async fn pending_ingress(&self) -> Result<Vec<FeishuIngressRow>, DbError> {
        Ok(
            sqlx::query_as("SELECT id,payload FROM feishu_ingress_events WHERE done=0 ORDER BY rowid LIMIT 100")
                .fetch_all(&self.pool)
                .await?,
        )
    }
    async fn complete_ingress(&self, id: &str) -> Result<(), DbError> {
        sqlx::query("UPDATE feishu_ingress_events SET done=1 WHERE id=?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    async fn start_inbox_attempt(&self, id: &str, epoch: &str) -> Result<(), DbError> {
        sqlx::query("UPDATE feishu_inbox SET attempt_epoch=? WHERE id=?")
            .bind(epoch)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    async fn park_expired_outbox(&self, before: i64) -> Result<u64, DbError> {
        Ok(sqlx::query("UPDATE feishu_outbox SET delivery_state='unknown' WHERE delivery_state='pending' AND platform_message_id IS NULL AND first_attempt_at<=?").bind(before).execute(&self.pool).await?.rows_affected())
    }
    async fn park_outbox(&self, id: &str) -> Result<(), DbError> {
        sqlx::query("UPDATE feishu_outbox SET delivery_state='unknown' WHERE id=?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    async fn claim_message(&self, app: &str, message: &str, now: i64) -> Result<bool, DbError> {
        let changed = sqlx::query(
            "INSERT OR IGNORE INTO feishu_message_receipts (app_id, message_id, received_at) VALUES (?, ?, ?)",
        )
        .bind(app)
        .bind(message)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(changed.rows_affected() > 0)
    }
}
