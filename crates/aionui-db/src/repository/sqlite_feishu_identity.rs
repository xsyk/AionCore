use sqlx::SqlitePool;

use aionui_common::TimestampMs;

use crate::error::DbError;
use crate::models::FeishuIdentityTransactionRow;
use crate::repository::feishu_identity::{
    ConsumeFeishuIdentityTransactionParams, IFeishuIdentityTransactionRepository,
};

const COLUMNS: &str = "transaction_id,node_id,kind,nonce_hash,browser_binding_hash,redirect_uri,expires_at,consumed_at";

fn valid_opaque(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn valid_context(node_id: &str, redirect_uri: &str) -> bool {
    !node_id.is_empty()
        && node_id.len() <= 512
        && !node_id.chars().any(|value| value.is_whitespace() || value.is_control())
        && !redirect_uri.is_empty()
        && redirect_uri.len() <= 2048
        && !redirect_uri.chars().any(char::is_control)
}

#[derive(Clone, Debug)]
pub struct SqliteFeishuIdentityTransactionRepository {
    pool: SqlitePool,
}

impl SqliteFeishuIdentityTransactionRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IFeishuIdentityTransactionRepository for SqliteFeishuIdentityTransactionRepository {
    async fn insert(&self, row: &FeishuIdentityTransactionRow) -> Result<(), DbError> {
        if !valid_opaque(&row.transaction_id)
            || !valid_opaque(&row.nonce_hash)
            || !valid_opaque(&row.browser_binding_hash)
            || !valid_context(&row.node_id, &row.redirect_uri)
            || row.expires_at <= 0
            || row.consumed_at.is_some()
        {
            return Err(DbError::Init("FEISHU_IDENTITY_TRANSACTION_INVALID".into()));
        }
        sqlx::query(
            "INSERT INTO feishu_identity_transactions \
             (transaction_id,node_id,kind,nonce_hash,browser_binding_hash,redirect_uri,expires_at,consumed_at) \
             VALUES (?,?,?,?,?,?,?,NULL)",
        )
        .bind(&row.transaction_id)
        .bind(&row.node_id)
        .bind(row.kind.as_str())
        .bind(&row.nonce_hash)
        .bind(&row.browser_binding_hash)
        .bind(&row.redirect_uri)
        .bind(row.expires_at)
        .execute(&self.pool)
        .await
        .map_err(|error| {
            let error = DbError::from(error);
            if error.is_unique_violation() {
                DbError::Conflict("FEISHU_IDENTITY_TRANSACTION_EXISTS".into())
            } else {
                error
            }
        })?;
        Ok(())
    }

    async fn get(&self, transaction_id: &str) -> Result<Option<FeishuIdentityTransactionRow>, DbError> {
        Ok(sqlx::query_as::<_, FeishuIdentityTransactionRow>(&format!(
            "SELECT {COLUMNS} FROM feishu_identity_transactions WHERE transaction_id=?"
        ))
        .bind(transaction_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn consume(&self, params: &ConsumeFeishuIdentityTransactionParams) -> Result<bool, DbError> {
        if !valid_opaque(&params.transaction_id)
            || !valid_opaque(&params.nonce_hash)
            || !valid_opaque(&params.browser_binding_hash)
            || !valid_context(&params.node_id, &params.redirect_uri)
            || params.now_ms <= 0
        {
            return Ok(false);
        }
        // A single UPDATE is the admission boundary. No caller can observe a
        // valid row and then race another caller into issuing a second session.
        let changed = sqlx::query(
            "UPDATE feishu_identity_transactions SET consumed_at=? \
             WHERE transaction_id=? AND node_id=? AND kind=? AND nonce_hash=? \
               AND browser_binding_hash=? AND redirect_uri=? AND expires_at>? AND consumed_at IS NULL",
        )
        .bind(params.now_ms)
        .bind(&params.transaction_id)
        .bind(&params.node_id)
        .bind(params.kind.as_str())
        .bind(&params.nonce_hash)
        .bind(&params.browser_binding_hash)
        .bind(&params.redirect_uri)
        .bind(params.now_ms)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(changed == 1)
    }

    async fn delete_expired(&self, now_ms: TimestampMs) -> Result<u64, DbError> {
        Ok(
            sqlx::query("DELETE FROM feishu_identity_transactions WHERE expires_at<=?")
                .bind(now_ms)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }
}
