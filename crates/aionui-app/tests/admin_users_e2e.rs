//! End-to-end tests for super-admin user management and act-as.
//!
//! Runs the full router in webui identity mode (CSRF disabled so requests can
//! focus on authorization) against an in-memory database.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aionui_app::{AppConfig, AppServices};
use aionui_db::models::ConversationRow;

const ADMIN_ID: &str = "system_default_user";
const ADMIN_PW: &str = "AdminP@ss123";
const ACT_AS: &str = "x-aionui-act-as";

struct Ctx {
    app: axum::Router,
    services: AppServices,
}

async fn setup() -> (Ctx, String) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = AppConfig {
        disable_csrf: true,
        ..Default::default()
    };
    let services = AppServices::from_config(db, &config).await.unwrap();
    let app = aionui_app::create_router(&services).await.expect("build router");
    let hash = aionui_auth::hash_password(ADMIN_PW).unwrap();
    services
        .user_repo
        .set_system_user_credentials("admin", &hash)
        .await
        .unwrap();
    let ctx = Ctx { app, services };
    let token = login(&ctx, "admin", ADMIN_PW).await.expect("admin login");
    (ctx, token)
}

async fn send(
    ctx: &Ctx,
    method: &str,
    uri: &str,
    token: Option<&str>,
    act_as: Option<&str>,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if let Some(target) = act_as {
        builder = builder.header(ACT_AS, target);
    }
    let req = match body {
        Some(json) => builder
            .header("content-type", "application/json")
            .body(Body::from(json.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let resp = ctx.app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn login(ctx: &Ctx, username: &str, password: &str) -> Option<String> {
    let (status, json) = send(
        ctx,
        "POST",
        "/login",
        None,
        None,
        Some(serde_json::json!({"username": username, "password": password})),
    )
    .await;
    (status == StatusCode::OK).then(|| json["token"].as_str().unwrap().to_owned())
}

async fn create_user(ctx: &Ctx, admin: &str, username: &str, password: &str) -> String {
    let (status, json) = send(
        ctx,
        "POST",
        "/api/admin/users",
        Some(admin),
        None,
        Some(serde_json::json!({"username": username, "password": password})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create {username}: {json}");
    assert_eq!(json["data"]["username"], username);
    assert!(json["data"].get("password_hash").is_none(), "no secrets in response");
    json["data"]["id"].as_str().unwrap().to_owned()
}

async fn seed_conversation(ctx: &Ctx, user_id: &str, id: &str) {
    let now = aionui_common::now_ms();
    ctx.services
        .conversation_repo
        .create(&ConversationRow {
            id: id.to_owned(),
            user_id: user_id.to_owned(),
            name: format!("conv of {user_id}"),
            r#type: "acp".to_owned(),
            extra: r#"{"backend":"claude"}"#.to_owned(),
            model: None,
            status: Some("finished".to_owned()),
            source: Some("aionui".to_owned()),
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            created_at: now,
            updated_at: now,
            project_id: None,
            folder_id: None,
            name_source: None,
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn admin_creates_user_who_can_log_in() {
    let (ctx, admin) = setup().await;
    create_user(&ctx, &admin, "alice", "AliceP@ss123").await;
    assert!(login(&ctx, "alice", "AliceP@ss123").await.is_some());

    let (status, json) = send(&ctx, "GET", "/api/admin/users", Some(&admin), None, None).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<_> = json["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["username"].clone())
        .collect();
    assert!(names.contains(&serde_json::json!("alice")));
    assert_eq!(json["data"][0]["is_super_admin"], true, "super admin listed first");
}

#[tokio::test]
async fn create_user_rejects_duplicate_and_invalid_input() {
    let (ctx, admin) = setup().await;
    create_user(&ctx, &admin, "alice", "AliceP@ss123").await;
    let (status, _) = send(
        &ctx,
        "POST",
        "/api/admin/users",
        Some(&admin),
        None,
        Some(serde_json::json!({"username": "alice", "password": "AliceP@ss123"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = send(
        &ctx,
        "POST",
        "/api/admin/users",
        Some(&admin),
        None,
        Some(serde_json::json!({"username": "bob", "password": "x"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn admin_lists_every_users_conversations_with_owner() {
    let (ctx, admin) = setup().await;
    let alice = create_user(&ctx, &admin, "alice", "AliceP@ss123").await;
    seed_conversation(&ctx, &alice, "conv_alice").await;
    seed_conversation(&ctx, ADMIN_ID, "conv_admin").await;

    let (status, json) = send(&ctx, "GET", "/api/admin/conversations", Some(&admin), None, None).await;
    assert_eq!(status, StatusCode::OK);
    let items = json["data"].as_array().unwrap();
    assert_eq!(items.len(), 1, "admin's own conversations are excluded: {json}");
    assert_eq!(items[0]["id"], "conv_alice");
    assert_eq!(items[0]["backend"], "claude");
    assert_eq!(items[0]["owner"]["username"], "alice");
    assert_eq!(items[0]["owner"]["deleted"], false);
}

#[tokio::test]
async fn admin_acts_as_owner_to_reach_their_conversation() {
    let (ctx, admin) = setup().await;
    let alice = create_user(&ctx, &admin, "alice", "AliceP@ss123").await;
    seed_conversation(&ctx, &alice, "conv_alice").await;

    let (status, _) = send(&ctx, "GET", "/api/conversations/conv_alice", Some(&admin), None, None).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "without act-as the admin is scoped to itself"
    );

    let (status, json) = send(
        &ctx,
        "GET",
        "/api/conversations/conv_alice",
        Some(&admin),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["data"]["id"], "conv_alice");

    let (status, json) = send(&ctx, "GET", "/api/auth/user", Some(&admin), Some(&alice), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["user"]["id"], alice.as_str());
    assert_eq!(json["user"]["is_super_admin"], true, "flag follows the real caller");
}

#[tokio::test]
async fn non_admin_cannot_act_as_or_use_admin_endpoints() {
    let (ctx, admin) = setup().await;
    let alice = create_user(&ctx, &admin, "alice", "AliceP@ss123").await;
    let bob = create_user(&ctx, &admin, "bob", "BobP@ss12345").await;
    let alice_token = login(&ctx, "alice", "AliceP@ss123").await.unwrap();

    let (status, _) = send(&ctx, "GET", "/api/conversations", Some(&alice_token), Some(&bob), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = send(&ctx, "GET", "/api/admin/users", Some(&alice_token), None, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = send(&ctx, "GET", "/api/admin/conversations", Some(&alice_token), None, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = send(&ctx, "GET", "/api/admin/users", None, None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, json) = send(&ctx, "GET", "/api/auth/user", Some(&alice_token), None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["user"]["is_super_admin"], false);

    // Admin endpoints refuse act-as even for the super admin.
    let (status, _) = send(&ctx, "GET", "/api/admin/users", Some(&admin), Some(&alice), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn disable_enable_and_reset_password_revoke_sessions() {
    let (ctx, admin) = setup().await;
    let alice = create_user(&ctx, &admin, "alice", "AliceP@ss123").await;
    let old = login(&ctx, "alice", "AliceP@ss123").await.unwrap();

    let (status, json) = send(
        &ctx,
        "POST",
        &format!("/api/admin/users/{alice}/disable"),
        Some(&admin),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["data"]["status"], "disabled");
    let (status, _) = send(&ctx, "GET", "/api/conversations", Some(&old), None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(login(&ctx, "alice", "AliceP@ss123").await.is_none());

    let (status, _) = send(
        &ctx,
        "POST",
        &format!("/api/admin/users/{alice}/enable"),
        Some(&admin),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let fresh = login(&ctx, "alice", "AliceP@ss123").await.expect("login after enable");

    let (status, _) = send(
        &ctx,
        "POST",
        &format!("/api/admin/users/{alice}/password"),
        Some(&admin),
        None,
        Some(serde_json::json!({"password": "NewAliceP@ss456"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(&ctx, "GET", "/api/conversations", Some(&fresh), None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "reset revokes live sessions");
    assert!(login(&ctx, "alice", "AliceP@ss123").await.is_none());
    assert!(login(&ctx, "alice", "NewAliceP@ss456").await.is_some());
}

#[tokio::test]
async fn soft_delete_keeps_data_and_frees_username() {
    let (ctx, admin) = setup().await;
    let alice = create_user(&ctx, &admin, "alice", "AliceP@ss123").await;
    seed_conversation(&ctx, &alice, "conv_alice").await;

    let (status, _) = send(
        &ctx,
        "DELETE",
        &format!("/api/admin/users/{alice}"),
        Some(&admin),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(login(&ctx, "alice", "AliceP@ss123").await.is_none());

    let (status, json) = send(&ctx, "GET", "/api/admin/users", Some(&admin), None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !json["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["id"] == alice.as_str())
    );

    let (status, json) = send(&ctx, "GET", "/api/admin/conversations", Some(&admin), None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["data"][0]["id"], "conv_alice");
    assert_eq!(json["data"][0]["owner"]["deleted"], true);

    let (status, _) = send(
        &ctx,
        "GET",
        "/api/conversations/conv_alice",
        Some(&admin),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "deleted users cannot be acted as");

    let again = create_user(&ctx, &admin, "alice", "AliceP@ss789").await;
    assert_ne!(again, alice);

    let (status, _) = send(
        &ctx,
        "DELETE",
        &format!("/api/admin/users/{alice}"),
        Some(&admin),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "already deleted");
}

#[tokio::test]
async fn super_admin_cannot_be_disabled_or_deleted() {
    let (ctx, admin) = setup().await;
    let (status, _) = send(
        &ctx,
        "POST",
        &format!("/api/admin/users/{ADMIN_ID}/disable"),
        Some(&admin),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = send(
        &ctx,
        "DELETE",
        &format!("/api/admin/users/{ADMIN_ID}"),
        Some(&admin),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(login(&ctx, "admin", ADMIN_PW).await.is_some());
}
