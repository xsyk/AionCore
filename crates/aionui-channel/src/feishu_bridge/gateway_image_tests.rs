use super::*;

const PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 4, 0, 0, 0, 181, 28, 12,
    2, 0, 0, 0, 11, 73, 68, 65, 84, 120, 218, 99, 252, 255, 31, 0, 3, 3, 2, 0, 239, 191, 105, 151, 0, 0, 0, 0, 73, 69,
    78, 68, 174, 66, 96, 130,
];

fn image_delivery(route: &GatewayRoute, id: &str, text: &str, keys: &[&str]) -> GatewayDelivery {
    let mut event = message_event(id, text);
    event["message_type"] = "post".into();
    event["platform_message_id"] = format!("om_{id}").into();
    event["attachments"] = json!(
        keys.iter()
            .map(
                |key| json!({"kind":"image","file_key":key,"message_id":format!("om_{id}"),"local_path":"/etc/passwd"})
            )
            .collect::<Vec<_>>()
    );
    delivery(route, id, "message", Some(event))
}

#[tokio::test]
async fn gateway_image_only_delivery_preserves_references_and_requires_actual_group_mention() {
    let mut route = route();
    route.status = "active".into();
    let original = image_delivery(&route, "image_only", "", &["img_one"]);
    let event = serde_json::to_value(original.event.as_ref().unwrap()).unwrap();
    assert_eq!(event["attachments"][0]["file_key"], "img_one");
    assert_eq!(event["attachments"][0]["file_name"], "");
    assert!(event["attachments"][0].get("local_path").is_none());
    assert!(
        crate::feishu_bridge::gateway_runtime::validate_gateway_delivery("node", "app", "tenant", &original).is_ok()
    );
    let mut unmentioned = serde_json::to_value(&original).unwrap();
    unmentioned["event"]["mentioned_bot"] = false.into();
    let unmentioned: GatewayDelivery = serde_json::from_value(unmentioned).unwrap();
    assert!(
        crate::feishu_bridge::gateway_runtime::validate_gateway_delivery("node", "app", "tenant", &unmentioned)
            .is_err()
    );
    let mut mismatched = serde_json::to_value(&original).unwrap();
    mismatched["event"]["attachments"][0]["message_id"] = "om_someone_else".into();
    let mismatched: GatewayDelivery = serde_json::from_value(mismatched).unwrap();
    assert!(
        crate::feishu_bridge::gateway_runtime::validate_gateway_delivery("node", "app", "tenant", &mismatched).is_err()
    );
    let mut p2p = serde_json::to_value(&original).unwrap();
    p2p["route"]["chat_type"] = "p2p".into();
    p2p["event"]["chat_type"] = "p2p".into();
    p2p["event"]["mentioned_bot"] = false.into();
    let p2p: GatewayDelivery = serde_json::from_value(p2p).unwrap();
    assert!(crate::feishu_bridge::gateway_runtime::validate_gateway_delivery("node", "app", "tenant", &p2p).is_ok());
    let excessive = image_delivery(&route, "many", "", &["img"; 11]);
    assert!(
        crate::feishu_bridge::gateway_runtime::validate_gateway_delivery("node", "app", "tenant", &excessive)
            .unwrap_err()
            .to_string()
            .contains("GATEWAY_IMAGE_COUNT_INVALID")
    );
}

#[tokio::test]
async fn gateway_image_failure_survives_restart_and_later_text_progresses_then_retries_with_files() {
    let (mut fixture, hub, url, task) = gateway_fixture(false).await;
    fixture.bridge.flush_gateway().await.unwrap();
    let route = hub.route.lock().unwrap().clone();
    let image = image_delivery(&route, "retry_image", "caption", &["img_retry"]);
    fixture.bridge.accept_gateway_delivery(&image).await.unwrap();
    let raw = hub.repository.pending_deliveries().await.unwrap()[0].payload.clone();
    assert!(
        raw.contains("img_retry"),
        "durable original attachment reference was lost"
    );
    assert_eq!(hub.resource_calls.load(Ordering::SeqCst), 0);
    fixture.bridge = bridge_with_gateway(&fixture, &url, hub.repository.clone());
    hub.deliveries.lock().unwrap().push(delivery(
        &route,
        "later_text",
        "message",
        Some(message_event("later_text", "later text")),
    ));
    fixture.bridge.flush_gateway().await.unwrap();
    let pending = hub.repository.pending_deliveries().await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, "retry_image");
    assert_eq!(pending[0].payload, raw);
    let inbox = fixture.bindings.pending_inbox().await.unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].id, "feishu:app:later_text");
    hub.resources
        .lock()
        .unwrap()
        .insert("img_retry".into(), ("image/png".into(), PNG.to_vec()));
    fixture.bridge.flush_gateway().await.unwrap();
    assert!(hub.repository.pending_deliveries().await.unwrap().is_empty());
    let files: String = sqlx::query_scalar("SELECT files FROM feishu_inbox WHERE id='feishu:app:retry_image'")
        .fetch_one(fixture._database.pool())
        .await
        .unwrap();
    let files: Vec<String> = serde_json::from_str(&files).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(std::fs::read(&files[0]).unwrap(), PNG);
    assert!(files[0].contains(&aionui_common::user_upload_dir_name(&fixture.owner).unwrap()));
    fixture.bridge.accept_gateway_delivery(&image).await.unwrap();
    fixture.bridge.flush_gateway().await.unwrap();
    fixture.bridge.flush_inbox().await.unwrap();
    // The recording agent keeps the earlier text turn open until a terminal
    // event. The image must remain queued while that conversation is busy.
    assert_eq!(
        fixture.bindings.pending_inbox().await.unwrap()[0].id,
        "feishu:app:retry_image"
    );
    assert!(
        fixture
            .bridge
            .deps
            .conversation_repo
            .get_message(&fixture.owner, "test_conversation", "feishu:app:later_text")
            .await
            .unwrap()
            .is_some()
    );
    let runtime = fixture.bridge.deps.conversations.runtime_state();
    assert!(runtime.is_claimed("test_conversation"));
    tokio::time::timeout(Duration::from_secs(2), async {
        while fixture.agent.tx.receiver_count() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    fixture
        .agent
        .tx
        .send(serde_json::from_value(json!({"type":"finish","data":{}})).unwrap())
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while runtime.is_claimed("test_conversation") {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    fixture.bridge.flush_inbox().await.unwrap();
    let message = fixture
        .bridge
        .deps
        .conversation_repo
        .get_message(&fixture.owner, "test_conversation", "feishu:app:retry_image")
        .await
        .unwrap()
        .unwrap();
    assert!(message.content.contains("[[AION_FILES]]"));
    assert!(message.content.contains("caption"));
    assert!(!message.content.contains("/etc/passwd"));
    let saved: Value = serde_json::from_str(&message.content).unwrap();
    let saved_files: Vec<String> = serde_json::from_value(saved["files"].clone()).unwrap();
    let native = aionui_ai_agent::media::partition_media(
        saved["content"].as_str().unwrap(),
        &saved_files,
        aionui_ai_agent::types::PromptMediaCaps {
            image: true,
            audio: false,
        },
    );
    assert_eq!(native.media.len(), 1);
    assert!(native.path_files.is_empty());
    assert_eq!(native.media[0].mime, "image/png");
    for file in saved_files {
        std::fs::remove_file(file).unwrap();
    }
    std::fs::remove_file(&files[0]).unwrap();
    task.abort();
}

#[tokio::test]
async fn gateway_image_busy_retries_do_not_materialize_more_files() {
    let (fixture, hub, _, task) = gateway_fixture(false).await;
    fixture.bridge.flush_gateway().await.unwrap();
    hub.resources
        .lock()
        .unwrap()
        .insert("img_busy".into(), ("image/png".into(), PNG.to_vec()));
    let id = format!("busy_{}", aionui_common::generate_id());
    let image = image_delivery(&hub.route.lock().unwrap().clone(), &id, "caption", &["img_busy"]);
    fixture.bridge.accept_gateway_delivery(&image).await.unwrap();
    fixture.bridge.flush_gateway().await.unwrap();
    let row = fixture.bindings.pending_inbox().await.unwrap().remove(0);
    let files: Vec<String> = serde_json::from_str(&row.files).unwrap();
    let filename = std::path::Path::new(&files[0]).file_name().unwrap().to_str().unwrap();
    let target = std::env::temp_dir().join("aionui/test_conversation");
    let copies = || {
        std::fs::read_dir(&target)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(filename))
            .count()
    };
    let claim = fixture
        .bridge
        .deps
        .conversations
        .runtime_state()
        .try_claim_turn("test_conversation", "busy_image_turn")
        .unwrap();
    for _ in 0..3 {
        fixture.bridge.flush_inbox().await.unwrap();
        assert_eq!(fixture.bindings.pending_inbox().await.unwrap().len(), 1);
        assert_eq!(copies(), 0, "Busy retry copied an image before accepting its turn");
    }
    drop(claim);
    let (first, second) = tokio::join!(fixture.bridge.flush_inbox(), fixture.bridge.flush_inbox());
    first.unwrap();
    second.unwrap();
    assert!(fixture.bindings.pending_inbox().await.unwrap().is_empty());
    assert_eq!(
        copies(),
        1,
        "accepted input must preserve exactly one conversation copy"
    );
    fixture.bridge.flush_inbox().await.unwrap();
    assert_eq!(copies(), 1, "completed input must remain idempotent");
    for entry in std::fs::read_dir(&target).unwrap().flatten() {
        if entry.file_name().to_string_lossy().ends_with(filename) {
            std::fs::remove_file(entry.path()).unwrap();
        }
    }
    std::fs::remove_file(&files[0]).unwrap();
    task.abort();
}

#[tokio::test]
async fn gateway_image_backlog_does_not_hide_later_text_or_interactions() {
    let (fixture, hub, _, task) = gateway_fixture(false).await;
    fixture.bridge.flush_gateway().await.unwrap();
    let original = hub.route.lock().unwrap().clone();
    let mut conversation = fixture
        .bridge
        .deps
        .conversation_repo
        .get(&fixture.owner, "test_conversation")
        .await
        .unwrap()
        .unwrap();
    conversation.id = "backlog_healthy".into();
    conversation.user_id = fixture.other.clone();
    fixture
        .bridge
        .deps
        .conversation_repo
        .create(&conversation)
        .await
        .unwrap();
    let mut healthy = route();
    healthy.id = "backlog_route".into();
    healthy.generation = "backlog_generation".into();
    healthy.conversation_id = conversation.id.clone();
    healthy.owner_union_id = "on_other".into();
    healthy.chat_id = "backlog_group".into();
    hub.extra_routes
        .lock()
        .unwrap()
        .insert(conversation.id.clone(), healthy.clone());
    let prepare = delivery(&healthy, "backlog_prepare", "binding.prepare", None);
    hub.deliveries.lock().unwrap().push(prepare.clone());
    fixture.bridge.accept_gateway_delivery(&prepare).await.unwrap();
    healthy = hub.route_for_conversation(&conversation.id).unwrap();
    hub.repository
        .activate_route(&healthy.id, &healthy.generation)
        .await
        .unwrap();
    let binding = fixture
        .bindings
        .get_by_conversation(&fixture.other, &conversation.id)
        .await
        .unwrap()
        .unwrap();
    fixture.bridge.publish_question(&binding, "backlog_question", &json!({
        "request_id":"backlog_question", "questions":[{"question":"Which", "multi_select":true, "options":[{"label":"A"}]}]
    })).await.unwrap();
    fixture.bridge.flush_interactions().await;
    let token = fixture.bridge.interactions.lock().await.keys().next().unwrap().clone();
    let mut queued = Vec::new();
    for index in 0..100 {
        queued.push(image_delivery(
            &original,
            &format!("missing_{index:03}"),
            "",
            &["img_missing"],
        ));
    }
    for index in 0..105 {
        let id = format!("backlog_text_{index:03}");
        let mut event = message_event(&id, &id);
        event["chat_id"] = healthy.chat_id.clone().into();
        queued.push(delivery(&healthy, &id, "message", Some(event)));
    }
    let mut interaction = click(&healthy, "backlog_click", &token);
    let event = interaction.event.as_mut().unwrap();
    event.chat_id = healthy.chat_id.clone();
    event.action = "feishu.select".into();
    event.value =
        json!({"tag":"button","value":{"action":"feishu.select","params":{"token":token,"question":"0","option":"0"}}});
    queued.push(interaction);
    for delivery in queued {
        fixture.bridge.accept_gateway_delivery(&delivery).await.unwrap();
    }
    for _ in 0..3 {
        fixture.bridge.flush_gateway().await.unwrap();
    }
    let inputs: Vec<String> =
        sqlx::query_scalar("SELECT id FROM feishu_inbox WHERE binding_id='backlog_generation' ORDER BY rowid")
            .fetch_all(fixture._database.pool())
            .await
            .unwrap();
    assert_eq!(
        inputs,
        (0..105)
            .map(|index| format!("feishu:app:backlog_text_{index:03}"))
            .collect::<Vec<_>>(),
        "failed images hid later text or changed its durable order"
    );
    let interactions = hub.repository.list_interactions(now_ms()).await.unwrap();
    let state: Value = serde_json::from_str(&interactions[0].payload).unwrap();
    assert_eq!(interactions[0].binding_id, healthy.generation);
    assert_eq!(
        state["kind"]["Ask"][0]["selected"],
        json!(["A"]),
        "other conversation interaction was starved"
    );
    let pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM feishu_gateway_deliveries WHERE done=0")
        .fetch_one(fixture._database.pool())
        .await
        .unwrap();
    assert_eq!(pending, 100, "failed images must remain durable for retry");
    task.abort();
}

#[tokio::test]
async fn gateway_image_paging_rotates_resource_selection_across_equal_pages() {
    let (fixture, hub, _, task) = gateway_fixture(false).await;
    fixture.bridge.flush_gateway().await.unwrap();
    let route = hub.route.lock().unwrap().clone();
    hub.resources
        .lock()
        .unwrap()
        .insert("img_good".into(), ("image/png".into(), PNG.to_vec()));
    let prefix = aionui_common::generate_id();
    for index in 0..200 {
        let key = if index == 1 || index == 101 {
            "img_good"
        } else {
            "img_missing"
        };
        let image = image_delivery(&route, &format!("paging_{prefix}_{index}"), "", &[key]);
        fixture.bridge.accept_gateway_delivery(&image).await.unwrap();
    }
    for _ in 0..2 {
        fixture.bridge.flush_gateway().await.unwrap();
    }
    assert!(fixture.bindings.pending_inbox().await.unwrap().is_empty());
    for _ in 0..2 {
        fixture.bridge.flush_gateway().await.unwrap();
    }
    let rows = fixture.bindings.pending_inbox().await.unwrap();
    assert_eq!(
        rows.len(),
        2,
        "each page must rotate to its second image on the next pass"
    );
    assert_eq!(
        hub.resource_calls.load(Ordering::SeqCst),
        4,
        "each tick must keep its single-image budget"
    );
    for row in rows {
        let files: Vec<String> = serde_json::from_str(&row.files).unwrap();
        assert_eq!(files.len(), 1);
        std::fs::remove_file(&files[0]).unwrap();
    }
    task.abort();
}

#[tokio::test]
async fn gateway_image_download_does_not_hold_management_lock_and_rechecks_fence() {
    let (fixture, hub, _, task) = gateway_fixture(false).await;
    fixture.bridge.flush_gateway().await.unwrap();
    hub.resources
        .lock()
        .unwrap()
        .insert("img_blocked".into(), ("image/png".into(), PNG.to_vec()));
    hub.block_resource.store(true, Ordering::SeqCst);
    let image = image_delivery(
        &hub.route.lock().unwrap().clone(),
        "blocked_image",
        "caption",
        &["img_blocked"],
    );
    fixture.bridge.accept_gateway_delivery(&image).await.unwrap();
    let bridge = fixture.bridge.clone();
    let processing = tokio::spawn(async move { bridge.flush_gateway().await });
    tokio::time::timeout(Duration::from_secs(2), hub.resource_entered.notified())
        .await
        .unwrap();
    let management = tokio::time::timeout(Duration::from_secs(1), fixture.bridge.binding_management_lock.lock())
        .await
        .expect("download held management lock");
    hub.repository.fence_route("route", "generation").await.unwrap();
    drop(management);
    hub.resource_release.notify_one();
    processing.await.unwrap().unwrap();
    assert!(fixture.bindings.pending_inbox().await.unwrap().is_empty());
    task.abort();
}

#[tokio::test]
async fn gateway_image_only_p2p_keeps_attachment_order_and_fair_retry_does_not_starve_it() {
    let (fixture, hub, _, task) = gateway_fixture(false).await;
    hub.route.lock().unwrap().chat_type = "p2p".into();
    hub.deliveries.lock().unwrap()[0].route.chat_type = "p2p".into();
    fixture.bridge.flush_gateway().await.unwrap();
    let route = hub.route.lock().unwrap().clone();
    let mut bad = image_delivery(&route, "unavailable", "", &["img_missing"]);
    let mut good = image_delivery(&route, "only_images", "", &["img_second", "img_first"]);
    for delivery in [&mut bad, &mut good] {
        delivery.event.as_mut().unwrap().chat_type = "p2p".into();
        delivery.event.as_mut().unwrap().mentioned_bot = false;
        delivery.event.as_mut().unwrap().message_type = "image".into();
    }
    let mut second = PNG.to_vec();
    second.push(0);
    hub.resources
        .lock()
        .unwrap()
        .insert("img_second".into(), ("image/png".into(), second.clone()));
    hub.resources
        .lock()
        .unwrap()
        .insert("img_first".into(), ("image/png".into(), PNG.to_vec()));
    fixture.bridge.accept_gateway_delivery(&bad).await.unwrap();
    fixture.bridge.accept_gateway_delivery(&good).await.unwrap();
    fixture.bridge.flush_gateway().await.unwrap();
    assert!(fixture.bindings.pending_inbox().await.unwrap().is_empty());
    fixture.bridge.flush_gateway().await.unwrap();
    let rows = fixture.bindings.pending_inbox().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "feishu:app:only_images");
    let files: Vec<String> = serde_json::from_str(&rows[0].files).unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(std::fs::read(&files[0]).unwrap(), second);
    assert_eq!(std::fs::read(&files[1]).unwrap(), PNG);
    fixture.bridge.flush_inbox().await.unwrap();
    let message = fixture
        .bridge
        .deps
        .conversation_repo
        .get_message(&fixture.owner, "test_conversation", "feishu:app:only_images")
        .await
        .unwrap()
        .unwrap();
    assert!(message.content.contains("[[AION_FILES]]"));
    let saved: Value = serde_json::from_str(&message.content).unwrap();
    let saved_files: Vec<String> = serde_json::from_value(saved["files"].clone()).unwrap();
    for file in saved_files.into_iter().chain(files) {
        std::fs::remove_file(file).unwrap();
    }
    assert_eq!(hub.repository.pending_deliveries().await.unwrap()[0].id, "unavailable");
    task.abort();
}

#[tokio::test]
async fn gateway_image_invalid_bytes_never_fall_back_to_caption_and_expiry_is_explicit() {
    let (fixture, hub, _, task) = gateway_fixture(false).await;
    fixture.bridge.flush_gateway().await.unwrap();
    hub.resources.lock().unwrap().insert(
        "img_html".into(),
        ("image/png".into(), b"<html>not an image</html>".to_vec()),
    );
    let mut image = image_delivery(
        &hub.route.lock().unwrap().clone(),
        "invalid_image",
        "must retain image",
        &["img_html"],
    );
    image.expires_at = now_ms() + 60_000;
    fixture.bridge.accept_gateway_delivery(&image).await.unwrap();
    fixture.bridge.flush_gateway().await.unwrap();
    assert!(fixture.bindings.pending_inbox().await.unwrap().is_empty());
    assert_eq!(hub.repository.pending_deliveries().await.unwrap().len(), 1);
    let mut payload = serde_json::to_value(&image).unwrap();
    payload["expires_at"] = (now_ms() - 1).into();
    sqlx::query("UPDATE feishu_gateway_deliveries SET payload=? WHERE id=?")
        .bind(payload.to_string())
        .bind(&image.id)
        .execute(fixture._database.pool())
        .await
        .unwrap();
    fixture.bridge.flush_gateway().await.unwrap();
    let outcome: String = sqlx::query_scalar("SELECT outcome FROM feishu_gateway_deliveries WHERE id=?")
        .bind(&image.id)
        .fetch_one(fixture._database.pool())
        .await
        .unwrap();
    assert_eq!(outcome, "attachment_expired");
    assert!(fixture.bindings.pending_inbox().await.unwrap().is_empty());
    task.abort();
}
