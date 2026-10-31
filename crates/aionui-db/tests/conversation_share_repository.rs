use aionui_db::{
    DbError, IConversationRepository, IConversationShareRepository, MessagePageDirection, MessagePageParams,
    SqliteConversationRepository, SqliteConversationShareRepository, init_database_memory,
    models::{ConversationRow, MessageRow},
};

const OWNER: &str = "system_default_user";

fn conversation(id: &str) -> ConversationRow {
    ConversationRow {
        id: id.into(),
        user_id: OWNER.into(),
        name: "Shared conversation".into(),
        r#type: "acp".into(),
        extra: "{}".into(),
        model: None,
        status: Some("finished".into()),
        source: Some("aionui".into()),
        channel_chat_id: None,
        pinned: false,
        pinned_at: None,
        created_at: 100,
        updated_at: 100,
        project_id: None,
        folder_id: None,
        name_source: None,
    }
}

fn message(id: &str, conversation_id: &str, hidden: bool, time: i64) -> MessageRow {
    MessageRow {
        id: id.into(),
        conversation_id: conversation_id.into(),
        msg_id: None,
        r#type: "text".into(),
        content: serde_json::json!({"content": id}).to_string(),
        position: Some("left".into()),
        status: Some("finish".into()),
        hidden,
        created_at: time,
        backend_turn_id: None,
    }
}

#[tokio::test]
async fn disabling_and_reenabling_preserves_the_original_token() {
    let db = init_database_memory().await.unwrap();
    let conversations = SqliteConversationRepository::new(db.pool().clone());
    let shares = SqliteConversationShareRepository::new(db.pool().clone());
    conversations.create(&conversation("shared")).await.unwrap();

    assert!(shares.get_for_owner(OWNER, "shared").await.unwrap().is_none());
    let created = shares.set_enabled(OWNER, "shared", "first-token", true).await.unwrap();
    assert!(created.enabled);
    assert_eq!(created.token, "first-token");
    assert!(shares.find_public("first-token").await.unwrap().is_some());

    shares
        .set_enabled(OWNER, "shared", "replacement-token", false)
        .await
        .unwrap();
    assert!(shares.find_public("first-token").await.unwrap().is_none());
    let restored = shares
        .set_enabled(OWNER, "shared", "replacement-token", true)
        .await
        .unwrap();
    assert_eq!(restored.token, "first-token");
    assert!(shares.find_public("first-token").await.unwrap().is_some());
    assert!(shares.find_public("replacement-token").await.unwrap().is_none());
}

#[tokio::test]
async fn owner_scope_and_parent_deletion_are_enforced() {
    let db = init_database_memory().await.unwrap();
    let conversations = SqliteConversationRepository::new(db.pool().clone());
    let shares = SqliteConversationShareRepository::new(db.pool().clone());
    conversations.create(&conversation("shared")).await.unwrap();

    assert!(matches!(
        shares.get_for_owner("other-user", "shared").await.unwrap_err(),
        DbError::NotFound(_)
    ));
    assert!(matches!(
        shares
            .set_enabled("other-user", "shared", "stolen", true)
            .await
            .unwrap_err(),
        DbError::NotFound(_)
    ));
    shares.set_enabled(OWNER, "shared", "valid", true).await.unwrap();
    conversations.delete(OWNER, "shared").await.unwrap();
    assert!(shares.find_public("valid").await.unwrap().is_none());
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversation_shares")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn public_pages_are_live_visible_and_scoped_to_exactly_one_conversation() {
    let db = init_database_memory().await.unwrap();
    let conversations = SqliteConversationRepository::new(db.pool().clone());
    let shares = SqliteConversationShareRepository::new(db.pool().clone());
    conversations.create(&conversation("shared")).await.unwrap();
    conversations.create(&conversation("private")).await.unwrap();
    for row in [
        message("first", "shared", false, 1),
        message("hidden", "shared", true, 2),
        message("foreign", "private", false, 3),
        message("second", "shared", false, 4),
    ] {
        conversations.insert_message(OWNER, &row).await.unwrap();
    }
    shares.set_enabled(OWNER, "shared", "valid", true).await.unwrap();
    let params = MessagePageParams {
        limit: 1,
        direction: MessagePageDirection::InitialLatest,
    };
    let latest = shares.list_public_messages("valid", &params).await.unwrap();
    assert_eq!(
        latest.items.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        vec!["second"]
    );
    assert!(latest.has_more_before);
    assert!(!latest.has_more_after);

    let earlier = shares
        .list_public_messages(
            "valid",
            &MessagePageParams {
                limit: 20,
                direction: MessagePageDirection::Before {
                    cursor: (&latest.items[0]).into(),
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(
        earlier.items.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        vec!["first"]
    );
    assert!(!earlier.has_more_before);
    assert!(earlier.has_more_after);

    conversations
        .insert_message(OWNER, &message("new", "shared", false, 5))
        .await
        .unwrap();
    let refreshed = shares.list_public_messages("valid", &params).await.unwrap();
    assert_eq!(refreshed.items[0].id, "new");
    assert!(matches!(
        shares.list_public_messages("unknown", &params).await.unwrap_err(),
        DbError::NotFound(_)
    ));
    shares.set_enabled(OWNER, "shared", "ignored", false).await.unwrap();
    assert!(matches!(
        shares.list_public_messages("valid", &params).await.unwrap_err(),
        DbError::NotFound(_)
    ));
}
