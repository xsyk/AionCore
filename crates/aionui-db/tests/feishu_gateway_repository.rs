use aionui_db::models::FeishuGatewayRouteRow;
use aionui_db::{
    DbError, IFeishuBindingRepository, IFeishuGatewayRepository, SqliteFeishuBindingRepository,
    SqliteFeishuGatewayRepository, init_database_memory,
};

async fn fixture() -> (
    aionui_db::Database,
    SqliteFeishuGatewayRepository,
    SqliteFeishuBindingRepository,
) {
    let db = init_database_memory().await.unwrap();
    for (id, external) in [("owner_a", "feishu:on_owner_a"), ("owner_b", "feishu:on_owner_b")] {
        sqlx::query("INSERT INTO users (id,user_type,external_user_id,username,password_hash,created_at,updated_at) VALUES (?,'local',?,?,'hash',1,1)")
            .bind(id).bind(external).bind(id).execute(db.pool()).await.unwrap();
        sqlx::query(
            "INSERT INTO conversations (id,user_id,name,type,created_at,updated_at) VALUES (?,?,'test','acp',1,1)",
        )
        .bind(format!("conv_{id}"))
        .bind(id)
        .execute(db.pool())
        .await
        .unwrap();
    }
    let gateway = SqliteFeishuGatewayRepository::new(db.pool().clone());
    gateway.ensure_identity("node", "app", "tenant").await.unwrap();
    let legacy = SqliteFeishuBindingRepository::new(db.pool().clone());
    (db, gateway, legacy)
}

fn route() -> FeishuGatewayRouteRow {
    FeishuGatewayRouteRow {
        route_id: "r_a".into(),
        generation: "g_a".into(),
        node_id: "node".into(),
        conversation_id: "conv_owner_a".into(),
        owner_user_id: "owner_a".into(),
        owner_union_id: "on_owner_a".into(),
        app_id: "app".into(),
        tenant_key: "tenant".into(),
        chat_id: "chat".into(),
        chat_type: "group".into(),
        status: "prepared".into(),
        prepared_at: 100,
    }
}

async fn insert_message(db: &aionui_db::Database, id: &str, created: i64) {
    sqlx::query("INSERT INTO messages(id,conversation_id,msg_id,type,content,position,status,hidden,created_at) VALUES (?,'conv_owner_a',?,'text','{\"content\":\"test\"}','right','finish',0,?)")
        .bind(id).bind(id).bind(created).execute(db.pool()).await.unwrap();
}

#[tokio::test]
async fn identity_is_immutable_and_prepared_routes_cannot_be_used_as_bindings() {
    let (_db, gateway, legacy) = fixture().await;
    gateway.ensure_identity("node", "app", "tenant").await.unwrap();
    for (node, app, tenant) in [
        ("other", "app", "tenant"),
        ("node", "other", "tenant"),
        ("node", "app", "other"),
    ] {
        assert!(matches!(
            gateway.ensure_identity(node, app, tenant).await,
            Err(DbError::Conflict(_))
        ));
    }
    gateway.prepare_route(&route()).await.unwrap();
    assert!(legacy.list_bindings().await.unwrap().is_empty());
    assert!(legacy.get_by_chat("app", "tenant", "chat").await.unwrap().is_none());
    assert_eq!(
        gateway.route_by_generation("g_a").await.unwrap().unwrap().status,
        "prepared"
    );
}

#[tokio::test]
async fn activation_uses_the_first_prepare_snapshot_and_is_idempotent() {
    let (db, gateway, legacy) = fixture().await;
    insert_message(&db, "history", 100).await;
    gateway.prepare_route(&route()).await.unwrap();
    insert_message(&db, "after_prepare", 1).await;
    let mut retried = route();
    retried.prepared_at = 200;
    gateway.prepare_route(&retried).await.unwrap();
    let binding = gateway.activate_route("r_a", "g_a").await.unwrap();
    assert_eq!(binding.id, "g_a");
    assert_eq!(binding.connector_user_id, "owner_a");
    assert_eq!(gateway.activate_route("r_a", "g_a").await.unwrap().id, "g_a");
    assert_eq!(legacy.list_bindings().await.unwrap().len(), 1);
    let candidates = legacy.sync_candidates("g_a").await.unwrap();
    assert_eq!(
        candidates.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["after_prepare"]
    );
    assert_eq!(
        gateway.route_by_generation("g_a").await.unwrap().unwrap().prepared_at,
        100
    );
}

#[tokio::test]
async fn preparation_and_activation_revalidate_account_and_conversation_ownership() {
    let (db, gateway, _legacy) = fixture().await;
    let mut wrong = route();
    wrong.owner_user_id = "owner_b".into();
    wrong.owner_union_id = "on_owner_b".into();
    assert!(matches!(gateway.prepare_route(&wrong).await, Err(DbError::NotFound(_))));
    wrong = route();
    wrong.owner_union_id = "on_owner_b".into();
    assert!(matches!(gateway.prepare_route(&wrong).await, Err(DbError::NotFound(_))));
    gateway.prepare_route(&route()).await.unwrap();
    sqlx::query("UPDATE users SET status='disabled' WHERE id='owner_a'")
        .execute(db.pool())
        .await
        .unwrap();
    assert!(matches!(
        gateway.activate_route("r_a", "g_a").await,
        Err(DbError::NotFound(_))
    ));
    assert!(matches!(
        gateway.prepare_route(&route()).await,
        Err(DbError::NotFound(_))
    ));
    sqlx::query("UPDATE users SET status='active', deleted_at=1 WHERE id='owner_a'")
        .execute(db.pool())
        .await
        .unwrap();
    assert!(matches!(
        gateway.activate_route("r_a", "g_a").await,
        Err(DbError::NotFound(_))
    ));
}

#[tokio::test]
async fn fencing_is_durable_and_a_late_old_revoke_does_not_remove_a_new_generation() {
    let (_db, gateway, legacy) = fixture().await;
    gateway.prepare_route(&route()).await.unwrap();
    gateway.activate_route("r_a", "g_a").await.unwrap();
    gateway.save_interaction("old-card", "g_a", "{}", 1000).await.unwrap();
    let mut next = route();
    next.route_id = "r_b".into();
    next.generation = "g_b".into();
    gateway.prepare_route(&next).await.unwrap();
    assert!(matches!(
        gateway.activate_route("r_b", "g_b").await,
        Err(DbError::Conflict(_))
    ));
    gateway.fence_route("r_a", "g_a").await.unwrap();
    assert!(legacy.list_bindings().await.unwrap().is_empty());
    assert!(gateway.list_interactions(100).await.unwrap().is_empty());
    assert!(matches!(
        gateway.activate_route("r_a", "g_a").await,
        Err(DbError::Conflict(_))
    ));
    assert!(matches!(
        gateway.prepare_route(&route()).await,
        Err(DbError::Conflict(_))
    ));
    gateway.activate_route("r_b", "g_b").await.unwrap();
    gateway.fence_route("r_a", "g_a").await.unwrap();
    assert_eq!(legacy.list_bindings().await.unwrap()[0].id, "g_b");
    assert_eq!(
        gateway.route_by_generation("g_a").await.unwrap().unwrap().status,
        "fenced"
    );
}

#[tokio::test]
async fn immutable_route_identity_rejects_mutated_redelivery_and_cross_generation_controls() {
    let (_db, gateway, _legacy) = fixture().await;
    gateway.prepare_route(&route()).await.unwrap();
    let mut changed = route();
    changed.chat_id = "another_chat".into();
    assert!(matches!(
        gateway.prepare_route(&changed).await,
        Err(DbError::Conflict(_))
    ));
    changed = route();
    changed.node_id = "another_node".into();
    assert!(matches!(
        gateway.prepare_route(&changed).await,
        Err(DbError::Conflict(_))
    ));
    assert!(matches!(
        gateway.activate_route("r_a", "wrong").await,
        Err(DbError::Conflict(_))
    ));
    assert!(matches!(
        gateway.fence_route("r_a", "wrong").await,
        Err(DbError::Conflict(_))
    ));
}

#[tokio::test]
async fn delivery_deduplication_preserves_receipts_and_old_attempts_are_not_reexecuted() {
    let (_db, gateway, _legacy) = fixture().await;
    gateway
        .enqueue_delivery("d_a", "r_a", "g_a", "interaction", "{\"answer\":1}")
        .await
        .unwrap();
    gateway
        .enqueue_delivery("d_a", "r_a", "g_a", "interaction", "{\"answer\":1}")
        .await
        .unwrap();
    assert!(matches!(
        gateway
            .enqueue_delivery("d_a", "r_a", "g_a", "interaction", "{\"answer\":2}")
            .await,
        Err(DbError::Conflict(_))
    ));
    assert!(matches!(
        gateway
            .enqueue_delivery("d_a", "r_b", "g_a", "interaction", "{\"answer\":1}")
            .await,
        Err(DbError::Conflict(_))
    ));
    assert_eq!(gateway.pending_deliveries().await.unwrap().len(), 1);
    assert!(gateway.start_delivery("d_a", "epoch_a").await.unwrap());
    assert!(gateway.start_delivery("d_a", "epoch_a").await.unwrap());
    assert!(!gateway.start_delivery("d_a", "epoch_b").await.unwrap());
    gateway.complete_delivery("d_a", "accepted").await.unwrap();
    gateway
        .enqueue_delivery("d_a", "r_a", "g_a", "interaction", "{\"answer\":1}")
        .await
        .unwrap();
    assert!(gateway.pending_deliveries().await.unwrap().is_empty());
    assert!(!gateway.start_delivery("d_a", "epoch_a").await.unwrap());
}

#[tokio::test]
async fn interaction_snapshots_restore_selections_and_exclude_expired_or_fenced_bindings() {
    let (_db, gateway, _legacy) = fixture().await;
    gateway.prepare_route(&route()).await.unwrap();
    gateway.activate_route("r_a", "g_a").await.unwrap();
    gateway
        .save_interaction("card", "g_a", "{\"selected\":[]}", 1000)
        .await
        .unwrap();
    gateway
        .save_interaction("card", "g_a", "{\"selected\":[\"A\"]}", 1000)
        .await
        .unwrap();
    assert_eq!(
        gateway.list_interactions(999).await.unwrap()[0].payload,
        "{\"selected\":[\"A\"]}"
    );
    assert!(gateway.list_interactions(1000).await.unwrap().is_empty());
    assert!(matches!(
        gateway.save_interaction("card", "different", "{}", 1000).await,
        Err(DbError::Conflict(_))
    ));
    gateway.delete_interaction("card").await.unwrap();
    assert!(gateway.list_interactions(999).await.unwrap().is_empty());
    gateway.fence_route("r_a", "g_a").await.unwrap();
    assert!(matches!(
        gateway.save_interaction("late", "g_a", "{}", 1000).await,
        Err(DbError::Conflict(_))
    ));
}

#[tokio::test]
async fn prepare_and_activation_rollback_all_local_state_if_a_baseline_write_fails() {
    let (db, gateway, legacy) = fixture().await;
    insert_message(&db, "history", 100).await;
    sqlx::query("CREATE TRIGGER fail_prepare BEFORE INSERT ON feishu_gateway_baseline_messages BEGIN SELECT RAISE(FAIL,'baseline unavailable'); END")
        .execute(db.pool()).await.unwrap();
    assert!(matches!(gateway.prepare_route(&route()).await, Err(DbError::Query(_))));
    assert!(gateway.route_by_generation("g_a").await.unwrap().is_none());
    sqlx::query("DROP TRIGGER fail_prepare")
        .execute(db.pool())
        .await
        .unwrap();
    gateway.prepare_route(&route()).await.unwrap();
    sqlx::query("CREATE TRIGGER fail_activate BEFORE INSERT ON feishu_binding_baseline_messages BEGIN SELECT RAISE(FAIL,'baseline unavailable'); END")
        .execute(db.pool()).await.unwrap();
    assert!(matches!(
        gateway.activate_route("r_a", "g_a").await,
        Err(DbError::Query(_))
    ));
    assert!(legacy.list_bindings().await.unwrap().is_empty());
    assert_eq!(
        gateway.route_by_generation("g_a").await.unwrap().unwrap().status,
        "prepared"
    );
    sqlx::query("DROP TRIGGER fail_activate")
        .execute(db.pool())
        .await
        .unwrap();
    gateway.activate_route("r_a", "g_a").await.unwrap();
    assert_eq!(legacy.list_bindings().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_failed_local_fence_does_not_claim_revocation_and_deleted_conversations_keep_audit_identity() {
    let (db, gateway, legacy) = fixture().await;
    gateway.prepare_route(&route()).await.unwrap();
    gateway.activate_route("r_a", "g_a").await.unwrap();
    sqlx::query("CREATE TRIGGER fail_fence BEFORE DELETE ON feishu_conversation_bindings BEGIN SELECT RAISE(FAIL,'fence unavailable'); END")
        .execute(db.pool()).await.unwrap();
    assert!(matches!(
        gateway.fence_route("r_a", "g_a").await,
        Err(DbError::Query(_))
    ));
    assert_eq!(
        gateway.route_by_generation("g_a").await.unwrap().unwrap().status,
        "active"
    );
    assert_eq!(legacy.list_bindings().await.unwrap().len(), 1);
    sqlx::query("DROP TRIGGER fail_fence").execute(db.pool()).await.unwrap();
    sqlx::query("DELETE FROM conversations WHERE id='conv_owner_a'")
        .execute(db.pool())
        .await
        .unwrap();
    assert!(legacy.list_bindings().await.unwrap().is_empty());
    assert!(gateway.route_by_generation("g_a").await.unwrap().is_some());
    gateway.fence_route("r_a", "g_a").await.unwrap();
    assert_eq!(
        gateway.route_by_generation("g_a").await.unwrap().unwrap().status,
        "fenced"
    );
}
