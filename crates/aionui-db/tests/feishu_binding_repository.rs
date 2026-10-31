use aionui_db::models::{FeishuBindingCodeRow, RedeemFeishuBindingParams};
use aionui_db::{IFeishuBindingRepository, SqliteFeishuBindingRepository, init_database_memory};

async fn fixture() -> (aionui_db::Database, SqliteFeishuBindingRepository) {
    let db = init_database_memory().await.unwrap();
    for (id, external) in [("owner_a", "feishu:on_owner_a"), ("owner_b", "feishu:on_owner_b")] {
        sqlx::query("INSERT INTO users (id, user_type, external_user_id, username, password_hash, created_at, updated_at) VALUES (?, 'local', ?, ?, 'hash', 1, 1)")
            .bind(id).bind(external).bind(id).execute(db.pool()).await.unwrap();
        sqlx::query("INSERT INTO conversations (id, user_id, name, type, created_at, updated_at) VALUES (?, ?, 'test', 'acp', 1, 1)")
            .bind(format!("conv_{id}")).bind(id).execute(db.pool()).await.unwrap();
    }
    let repo = SqliteFeishuBindingRepository::new(db.pool().clone());
    (db, repo)
}

fn code(owner: &str, code: &str) -> FeishuBindingCodeRow {
    FeishuBindingCodeRow {
        code: code.into(),
        conversation_id: format!("conv_{owner}"),
        owner_user_id: owner.into(),
        expires_at: 1000,
    }
}

fn redeem<'a>(code: &'a str, external: &'a str, chat: &'a str) -> RedeemFeishuBindingParams<'a> {
    RedeemFeishuBindingParams {
        code,
        external_user_id: external,
        connector_user_id: "owner_a",
        app_id: "cli_app",
        tenant_key: "tenant",
        chat_id: chat,
        chat_type: "group",
        now: 100,
    }
}

#[tokio::test]
async fn binding_code_checks_identity_expiry_and_is_single_use() {
    let (_db, repo) = fixture().await;
    repo.save_code(&code("owner_a", "CODEA")).await.unwrap();
    let error = repo
        .redeem_code(redeem("CODEA", "feishu:on_owner_b", "chat_a"))
        .await
        .unwrap_err();
    assert!(matches!(error, aionui_db::DbError::NotFound(_)));
    let bound = repo
        .redeem_code(redeem("CODEA", "feishu:on_owner_a", "chat_a"))
        .await
        .unwrap();
    assert_eq!(bound.owner_user_id, "owner_a");
    assert_eq!(bound.conversation_id, "conv_owner_a");
    assert!(matches!(
        repo.redeem_code(redeem("CODEA", "feishu:on_owner_a", "chat_a")).await,
        Err(aionui_db::DbError::NotFound(_))
    ));
    repo.save_code(&code("owner_a", "EXPIRED")).await.unwrap();
    let mut request = redeem("EXPIRED", "feishu:on_owner_a", "chat_a");
    request.now = 1001;
    assert!(matches!(
        repo.redeem_code(request).await,
        Err(aionui_db::DbError::NotFound(_))
    ));
}

#[tokio::test]
async fn chat_binding_cannot_be_taken_over_by_another_owner() {
    let (_db, repo) = fixture().await;
    repo.save_code(&code("owner_a", "CODEA")).await.unwrap();
    repo.redeem_code(redeem("CODEA", "feishu:on_owner_a", "chat_a"))
        .await
        .unwrap();
    repo.save_code(&code("owner_b", "CODEB")).await.unwrap();
    assert!(matches!(
        repo.redeem_code(redeem("CODEB", "feishu:on_owner_b", "chat_a")).await,
        Err(aionui_db::DbError::Conflict(_))
    ));
    assert_eq!(
        repo.get_by_chat("cli_app", "tenant", "chat_a")
            .await
            .unwrap()
            .unwrap()
            .owner_user_id,
        "owner_a"
    );
    assert!(
        repo.get_by_conversation("owner_b", "conv_owner_a")
            .await
            .unwrap()
            .is_none()
    );
    assert!(!repo.delete_binding("owner_b", "conv_owner_a").await.unwrap());
}

#[tokio::test]
async fn rebind_keeps_one_to_one_and_cascades_on_conversation_deletion() {
    let (db, repo) = fixture().await;
    repo.save_code(&code("owner_a", "CODEA")).await.unwrap();
    let first = repo
        .redeem_code(redeem("CODEA", "feishu:on_owner_a", "chat_a"))
        .await
        .unwrap();
    repo.save_code(&code("owner_a", "CODEB")).await.unwrap();
    let second = repo
        .redeem_code(redeem("CODEB", "feishu:on_owner_a", "chat_b"))
        .await
        .unwrap();
    assert_ne!(first.id, second.id);
    assert!(repo.get_by_chat("cli_app", "tenant", "chat_a").await.unwrap().is_none());
    assert_eq!(
        repo.get_by_conversation("owner_a", "conv_owner_a")
            .await
            .unwrap()
            .unwrap()
            .chat_id,
        "chat_b"
    );
    sqlx::query("DELETE FROM conversations WHERE id = 'conv_owner_a'")
        .execute(db.pool())
        .await
        .unwrap();
    assert!(repo.get_by_chat("cli_app", "tenant", "chat_b").await.unwrap().is_none());
}

#[tokio::test]
async fn incoming_message_receipts_are_durable_and_app_scoped() {
    let (_db, repo) = fixture().await;
    assert!(repo.claim_message("app_a", "message_1", 100).await.unwrap());
    assert!(!repo.claim_message("app_a", "message_1", 101).await.unwrap());
    assert!(repo.claim_message("app_b", "message_1", 101).await.unwrap());
}

#[tokio::test]
async fn history_boundary_is_a_transactional_id_snapshot_and_rebind_cancels_old_queues() {
    use aionui_db::models::FeishuOutboxRow;
    let (db, repo) = fixture().await;
    sqlx::query("INSERT INTO messages(id,conversation_id,msg_id,type,content,position,status,hidden,created_at) VALUES('history','conv_owner_a','history','text','{\"content\":\"history\"}','right','finish',0,100)").execute(db.pool()).await.unwrap();
    repo.save_code(&code("owner_a", "CODEA")).await.unwrap();
    let binding = repo
        .redeem_code(redeem("CODEA", "feishu:on_owner_a", "chat_a"))
        .await
        .unwrap();
    sqlx::query("INSERT INTO messages(id,conversation_id,msg_id,type,content,position,status,hidden,created_at) VALUES('new','conv_owner_a','new','text','{\"content\":\"new\"}','right','finish',0,1)").execute(db.pool()).await.unwrap();
    let candidates = repo.sync_candidates(&binding.id).await.unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].id, "new");
    repo.enqueue_inbox("feishu:app:input", &binding.id, "input")
        .await
        .unwrap();
    let rows: Vec<_> = (0..2)
        .map(|part| FeishuOutboxRow {
            id: format!("part_{part}"),
            binding_id: binding.id.clone(),
            message_id: "new".into(),
            part,
            content: "part".into(),
            platform_message_id: None,
            first_attempt_at: None,
        })
        .collect();
    sqlx::query("CREATE TRIGGER reject_second_part BEFORE INSERT ON feishu_outbox WHEN NEW.part=1 BEGIN SELECT RAISE(FAIL,'queue unavailable'); END").execute(db.pool()).await.unwrap();
    assert!(repo.enqueue_outbox(&rows).await.is_err());
    assert!(repo.pending_outbox().await.unwrap().is_empty());
    sqlx::query("DROP TRIGGER reject_second_part")
        .execute(db.pool())
        .await
        .unwrap();
    repo.enqueue_outbox(&rows).await.unwrap();
    assert_eq!(repo.pending_outbox().await.unwrap().len(), 2);
    repo.save_code(&code("owner_a", "CODEB")).await.unwrap();
    let rebound = repo
        .redeem_code(redeem("CODEB", "feishu:on_owner_a", "chat_b"))
        .await
        .unwrap();
    assert!(repo.pending_outbox().await.unwrap().is_empty());
    assert!(repo.pending_inbox().await.unwrap().is_empty());
    assert!(repo.sync_candidates(&rebound.id).await.unwrap().is_empty());
}
