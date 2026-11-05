use crate::models::{FeishuBindingRow, FeishuGatewayDeliveryRow, FeishuGatewayInteractionRow, FeishuGatewayRouteRow};
use crate::{DbError, IFeishuGatewayRepository, SqlitePool};

pub struct SqliteFeishuGatewayRepository {
    pool: SqlitePool,
}

impl SqliteFeishuGatewayRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

fn conflict(message: &str) -> DbError {
    DbError::Conflict(message.into())
}

fn valid_payload(payload: &str) -> bool {
    payload.len() <= 1_048_576
        && serde_json::from_str::<serde_json::Value>(payload).is_ok_and(|value| value.is_object())
}

fn same_route(left: &FeishuGatewayRouteRow, right: &FeishuGatewayRouteRow) -> bool {
    // The first preparation timestamp is a local baseline boundary. A retry
    // may arrive at a different wall-clock time, but may not replace identity.
    [
        &left.route_id,
        &left.generation,
        &left.node_id,
        &left.conversation_id,
        &left.owner_user_id,
        &left.owner_union_id,
        &left.app_id,
        &left.tenant_key,
        &left.chat_id,
        &left.chat_type,
    ] == [
        &right.route_id,
        &right.generation,
        &right.node_id,
        &right.conversation_id,
        &right.owner_user_id,
        &right.owner_union_id,
        &right.app_id,
        &right.tenant_key,
        &right.chat_id,
        &right.chat_type,
    ]
}

fn binding_matches(binding: &FeishuBindingRow, route: &FeishuGatewayRouteRow) -> bool {
    binding.id == route.generation
        && binding.conversation_id == route.conversation_id
        && binding.owner_user_id == route.owner_user_id
        && binding.connector_user_id == route.owner_user_id
        && binding.app_id == route.app_id
        && binding.tenant_key == route.tenant_key
        && binding.chat_id == route.chat_id
        && binding.chat_type == route.chat_type
}

async fn lock_identity(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>) -> Result<(), DbError> {
    // Take the SQLite write reservation before any state transition reads.
    let updated = sqlx::query("UPDATE feishu_gateway_identity SET node_id=node_id WHERE id=1")
        .execute(&mut **tx)
        .await?;
    if updated.rows_affected() == 0 {
        return Err(conflict("Gateway identity has not been configured"));
    }
    Ok(())
}

async fn validate_owner(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    route: &FeishuGatewayRouteRow,
) -> Result<(), DbError> {
    let valid: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversations c JOIN users u ON u.id=c.user_id WHERE c.id=? AND c.user_id=? AND u.external_user_id=? AND u.status='active' AND u.deleted_at IS NULL")
        .bind(&route.conversation_id).bind(&route.owner_user_id)
        .bind(format!("feishu:{}", route.owner_union_id)).fetch_one(&mut **tx).await?;
    if valid != 1 {
        return Err(DbError::NotFound("Gateway conversation owner is unavailable".into()));
    }
    Ok(())
}

async fn route_for_control(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    route_id: &str,
    generation: &str,
) -> Result<FeishuGatewayRouteRow, DbError> {
    let row: FeishuGatewayRouteRow = sqlx::query_as("SELECT * FROM feishu_gateway_routes WHERE route_id=?")
        .bind(route_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| DbError::NotFound("Gateway route is unavailable".into()))?;
    if row.generation != generation {
        return Err(conflict("Gateway route generation does not match"));
    }
    Ok(row)
}

#[async_trait::async_trait]
impl IFeishuGatewayRepository for SqliteFeishuGatewayRepository {
    async fn ensure_identity(&self, node_id: &str, app_id: &str, tenant_key: &str) -> Result<(), DbError> {
        if [node_id, app_id, tenant_key].iter().any(|part| part.is_empty()) {
            return Err(conflict("Gateway identity is incomplete"));
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT OR IGNORE INTO feishu_gateway_identity(id,node_id,app_id,tenant_key) VALUES(1,?,?,?)")
            .bind(node_id)
            .bind(app_id)
            .bind(tenant_key)
            .execute(&mut *tx)
            .await?;
        let existing: (String, String, String) =
            sqlx::query_as("SELECT node_id,app_id,tenant_key FROM feishu_gateway_identity WHERE id=1")
                .fetch_one(&mut *tx)
                .await?;
        if existing.0 != node_id || existing.1 != app_id || existing.2 != tenant_key {
            return Err(conflict("Gateway identity differs from the stored node identity"));
        }
        tx.commit().await?;
        Ok(())
    }

    async fn prepare_route(&self, route: &FeishuGatewayRouteRow) -> Result<(), DbError> {
        if route.status != "prepared"
            || !matches!(route.chat_type.as_str(), "p2p" | "group")
            || route.prepared_at < 0
            || [
                &route.route_id,
                &route.generation,
                &route.node_id,
                &route.conversation_id,
                &route.owner_user_id,
                &route.owner_union_id,
                &route.app_id,
                &route.tenant_key,
                &route.chat_id,
            ]
            .iter()
            .any(|part| part.is_empty())
        {
            return Err(conflict("Gateway preparation is incomplete"));
        }
        let mut tx = self.pool.begin().await?;
        lock_identity(&mut tx).await?;
        let identity: (String, String, String) =
            sqlx::query_as("SELECT node_id,app_id,tenant_key FROM feishu_gateway_identity WHERE id=1")
                .fetch_one(&mut *tx)
                .await?;
        if identity.0 != route.node_id || identity.1 != route.app_id || identity.2 != route.tenant_key {
            return Err(conflict("Gateway preparation belongs to another node identity"));
        }
        validate_owner(&mut tx, route).await?;
        let existing: Option<FeishuGatewayRouteRow> =
            sqlx::query_as("SELECT * FROM feishu_gateway_routes WHERE route_id=? OR generation=?")
                .bind(&route.route_id)
                .bind(&route.generation)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(existing) = existing {
            if !same_route(&existing, route) || existing.status == "fenced" {
                return Err(conflict("Gateway preparation conflicts with a stored generation"));
            }
            tx.commit().await?;
            return Ok(());
        }
        sqlx::query("INSERT INTO feishu_gateway_routes(route_id,generation,node_id,conversation_id,owner_user_id,owner_union_id,app_id,tenant_key,chat_id,chat_type,status,prepared_at) VALUES(?,?,?,?,?,?,?,?,?,?,'prepared',?)")
            .bind(&route.route_id).bind(&route.generation).bind(&route.node_id)
            .bind(&route.conversation_id).bind(&route.owner_user_id).bind(&route.owner_union_id)
            .bind(&route.app_id).bind(&route.tenant_key).bind(&route.chat_id).bind(&route.chat_type)
            .bind(route.prepared_at).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO feishu_gateway_baseline_messages(route_id,message_id) SELECT ?,id FROM messages WHERE conversation_id=?")
            .bind(&route.route_id).bind(&route.conversation_id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn activate_route(&self, route_id: &str, generation: &str) -> Result<FeishuBindingRow, DbError> {
        let mut tx = self.pool.begin().await?;
        lock_identity(&mut tx).await?;
        let route = route_for_control(&mut tx, route_id, generation).await?;
        if route.status == "fenced" {
            return Err(conflict("A fenced gateway generation cannot be activated"));
        }
        validate_owner(&mut tx, &route).await?;
        let conflicting: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM feishu_gateway_routes WHERE status='active' AND generation<>? AND (conversation_id=? OR (app_id=? AND tenant_key=? AND chat_id=?))")
            .bind(generation).bind(&route.conversation_id).bind(&route.app_id)
            .bind(&route.tenant_key).bind(&route.chat_id).fetch_one(&mut *tx).await?;
        let old_bindings: Vec<FeishuBindingRow> = sqlx::query_as("SELECT * FROM feishu_conversation_bindings WHERE id=? OR conversation_id=? OR (app_id=? AND tenant_key=? AND chat_id=?)")
            .bind(generation).bind(&route.conversation_id).bind(&route.app_id)
            .bind(&route.tenant_key).bind(&route.chat_id).fetch_all(&mut *tx).await?;
        if conflicting > 0 || old_bindings.iter().any(|binding| !binding_matches(binding, &route)) {
            return Err(conflict("A different local binding must be fenced before activation"));
        }
        let binding = FeishuBindingRow {
            id: generation.into(),
            conversation_id: route.conversation_id.clone(),
            owner_user_id: route.owner_user_id.clone(),
            connector_user_id: route.owner_user_id.clone(),
            app_id: route.app_id.clone(),
            tenant_key: route.tenant_key.clone(),
            chat_id: route.chat_id.clone(),
            chat_type: route.chat_type.clone(),
            bound_at: route.prepared_at,
        };
        if old_bindings.is_empty() {
            sqlx::query("INSERT INTO feishu_conversation_bindings(id,conversation_id,owner_user_id,connector_user_id,app_id,tenant_key,chat_id,chat_type,bound_at) VALUES(?,?,?,?,?,?,?,?,?)")
                .bind(&binding.id).bind(&binding.conversation_id).bind(&binding.owner_user_id)
                .bind(&binding.connector_user_id).bind(&binding.app_id).bind(&binding.tenant_key)
                .bind(&binding.chat_id).bind(&binding.chat_type).bind(binding.bound_at)
                .execute(&mut *tx).await?;
        }
        sqlx::query("INSERT OR IGNORE INTO feishu_binding_baseline_messages(binding_id,message_id) SELECT ?,message_id FROM feishu_gateway_baseline_messages WHERE route_id=?")
            .bind(generation).bind(route_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE feishu_gateway_routes SET status='active' WHERE route_id=? AND generation=?")
            .bind(route_id)
            .bind(generation)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(binding)
    }

    async fn fence_route(&self, route_id: &str, generation: &str) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        lock_identity(&mut tx).await?;
        let route = route_for_control(&mut tx, route_id, generation).await?;
        let binding: Option<FeishuBindingRow> = sqlx::query_as("SELECT * FROM feishu_conversation_bindings WHERE id=?")
            .bind(generation)
            .fetch_optional(&mut *tx)
            .await?;
        if binding
            .as_ref()
            .is_some_and(|binding| !binding_matches(binding, &route))
        {
            return Err(conflict("Gateway generation collides with another local binding"));
        }
        sqlx::query("UPDATE feishu_gateway_routes SET status='fenced' WHERE route_id=? AND generation=?")
            .bind(route_id)
            .bind(generation)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM feishu_gateway_interactions WHERE binding_id=?")
            .bind(generation)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM feishu_conversation_bindings WHERE id=?")
            .bind(generation)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn route_by_generation(&self, generation: &str) -> Result<Option<FeishuGatewayRouteRow>, DbError> {
        Ok(sqlx::query_as("SELECT * FROM feishu_gateway_routes WHERE generation=?")
            .bind(generation)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn list_routes(&self) -> Result<Vec<FeishuGatewayRouteRow>, DbError> {
        Ok(sqlx::query_as("SELECT * FROM feishu_gateway_routes ORDER BY rowid")
            .fetch_all(&self.pool)
            .await?)
    }

    async fn enqueue_delivery(
        &self,
        id: &str,
        route_id: &str,
        generation: &str,
        kind: &str,
        payload: &str,
    ) -> Result<(), DbError> {
        if [id, route_id, generation].iter().any(|part| part.is_empty())
            || !matches!(kind, "binding.prepare" | "binding.revoke" | "message" | "interaction")
            || !valid_payload(payload)
        {
            return Err(conflict("Gateway delivery receipt is invalid"));
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT OR IGNORE INTO feishu_gateway_deliveries(id,route_id,generation,kind,payload) VALUES(?,?,?,?,?)",
        )
        .bind(id)
        .bind(route_id)
        .bind(generation)
        .bind(kind)
        .bind(payload)
        .execute(&mut *tx)
        .await?;
        let existing: FeishuGatewayDeliveryRow = sqlx::query_as("SELECT * FROM feishu_gateway_deliveries WHERE id=?")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        if existing.route_id != route_id
            || existing.generation != generation
            || existing.kind != kind
            || existing.payload != payload
        {
            return Err(conflict("Gateway delivery identity was reused with different content"));
        }
        tx.commit().await?;
        Ok(())
    }

    async fn pending_deliveries(&self) -> Result<Vec<FeishuGatewayDeliveryRow>, DbError> {
        Ok(sqlx::query_as("SELECT id,route_id,generation,kind,payload,attempt_epoch FROM feishu_gateway_deliveries WHERE done=0 ORDER BY rowid LIMIT 100")
            .fetch_all(&self.pool).await?)
    }

    async fn start_delivery(&self, id: &str, epoch: &str) -> Result<bool, DbError> {
        if epoch.is_empty() {
            return Err(conflict("Gateway delivery process epoch is missing"));
        }
        Ok(sqlx::query("UPDATE feishu_gateway_deliveries SET attempt_epoch=COALESCE(attempt_epoch,?) WHERE id=? AND done=0 AND (attempt_epoch IS NULL OR attempt_epoch=?)")
            .bind(epoch).bind(id).bind(epoch).execute(&self.pool).await?.rows_affected() > 0)
    }

    async fn complete_delivery(&self, id: &str, outcome: &str) -> Result<(), DbError> {
        if outcome.is_empty() || outcome.len() > 128 {
            return Err(conflict("Gateway delivery outcome is invalid"));
        }
        let changed =
            sqlx::query("UPDATE feishu_gateway_deliveries SET done=1,outcome=? WHERE id=? AND (done=0 OR outcome=?)")
                .bind(outcome)
                .bind(id)
                .bind(outcome)
                .execute(&self.pool)
                .await?;
        if changed.rows_affected() == 0 {
            let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM feishu_gateway_deliveries WHERE id=?")
                .bind(id)
                .fetch_one(&self.pool)
                .await?;
            if exists == 0 {
                return Err(DbError::NotFound("Gateway delivery receipt is unavailable".into()));
            }
            return Err(conflict("Gateway delivery already has a different durable outcome"));
        }
        Ok(())
    }

    async fn save_interaction(
        &self,
        token: &str,
        binding_id: &str,
        payload: &str,
        expires_at: i64,
    ) -> Result<(), DbError> {
        if token.is_empty() || binding_id.is_empty() || expires_at <= 0 || !valid_payload(payload) {
            return Err(conflict("Gateway interaction snapshot is invalid"));
        }
        let changed = sqlx::query("INSERT INTO feishu_gateway_interactions(token,binding_id,payload,expires_at) SELECT ?,?,?,? WHERE EXISTS(SELECT 1 FROM feishu_gateway_routes g JOIN feishu_conversation_bindings b ON b.id=g.generation WHERE g.generation=? AND g.status='active') ON CONFLICT(token) DO UPDATE SET payload=excluded.payload WHERE feishu_gateway_interactions.binding_id=excluded.binding_id AND feishu_gateway_interactions.expires_at=excluded.expires_at")
            .bind(token).bind(binding_id).bind(payload).bind(expires_at).bind(binding_id)
            .execute(&self.pool).await?;
        if changed.rows_affected() == 0 {
            return Err(conflict(
                "Gateway interaction has a stale or different binding identity",
            ));
        }
        Ok(())
    }

    async fn list_interactions(&self, now: i64) -> Result<Vec<FeishuGatewayInteractionRow>, DbError> {
        Ok(sqlx::query_as("SELECT i.* FROM feishu_gateway_interactions i JOIN feishu_gateway_routes g ON g.generation=i.binding_id AND g.status='active' JOIN feishu_conversation_bindings b ON b.id=i.binding_id JOIN conversations c ON c.id=b.conversation_id AND c.user_id=b.owner_user_id JOIN users u ON u.id=b.owner_user_id WHERE i.expires_at>? AND u.status='active' AND u.deleted_at IS NULL ORDER BY i.rowid")
            .bind(now).fetch_all(&self.pool).await?)
    }

    async fn delete_interaction(&self, token: &str) -> Result<(), DbError> {
        sqlx::query("DELETE FROM feishu_gateway_interactions WHERE token=?")
            .bind(token)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
