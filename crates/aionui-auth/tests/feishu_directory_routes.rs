//! 仅验证新同步接口的权限和写请求边界，不验证账号登录流程。
use std::sync::Arc;

use aionui_auth::feishu::directory::service::FeishuDirectorySync;
use aionui_auth::{
    AuthIdentityMode, AuthRouterState, CookieConfig, JwtService, QrTokenStore, RefreshCoalescer, auth_routes,
};
use aionui_db::{IUserRepository, SqliteFeishuLoginRepository, SqliteUserRepository};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

async fn fixture() -> (Router, Arc<JwtService>, String, aionui_db::Database) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let users = Arc::new(SqliteUserRepository::new(db.pool().clone()));
    let regular = users.create_user("ordinary-member", "synthetic-hash").await.unwrap();
    let jwt = Arc::new(JwtService::new("synthetic-directory-jwt-key".into()));
    let service = Arc::new(FeishuDirectorySync::new(
        Arc::new(SqliteFeishuLoginRepository::new(db.pool().clone())),
        [42; 32],
        users.clone(),
        reqwest::Client::new(),
    ));
    let state = AuthRouterState {
        jwt_service: jwt.clone(),
        refresh_coalescer: RefreshCoalescer::new(),
        user_repo: users,
        fs_adopter: None,
        cookie_config: Arc::new(CookieConfig {
            secure: false,
            same_site: "Lax",
        }),
        qr_token_store: Arc::new(QrTokenStore::new()),
        identity_mode: AuthIdentityMode::UserSession,
        bootstrap_secret: None,
        session_revoked_hook: None,
        local: false,
        aionpro_mode: false,
        feishu: None,
        feishu_directory: Some(service),
    };
    (auth_routes(state), jwt, regular.id, db)
}

async fn call(
    app: Router,
    token: Option<String>,
    method: &str,
    csrf: bool,
    act_as: Option<&str>,
    body: &str,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri("/api/admin/feishu-directory/sync")
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    if csrf {
        request = request
            .header("cookie", "aionui-csrf-token=synthetic-csrf")
            .header("x-csrf-token", "synthetic-csrf");
    }
    if let Some(target) = act_as {
        request = request.header(aionui_auth::middleware::ACT_AS_HEADER, target);
    }
    let response = app
        .oneshot(request.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        },
    )
}

#[tokio::test]
async fn admin_user_list_exposes_display_name_but_keeps_login_and_secrets_private() {
    let (app, jwt, _, db) = fixture().await;
    let users = SqliteUserRepository::new(db.pool().clone());
    let account = users
        .create_external_local_user(
            "feishu:synthetic-admin-display",
            "feishu-kept-login",
            "synthetic-private-hash",
            aionui_db::UserStatus::Active,
        )
        .await
        .unwrap();
    users.update_display_name(&account.id, "合成列表姓名").await.unwrap();
    let token = jwt.sign("system_default_user", "admin").unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/admin/users")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&body).unwrap();
    let row = value["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == account.id)
        .unwrap();
    assert_eq!(row["username"], "feishu-kept-login");
    assert_eq!(row["display_name"], "合成列表姓名");
    assert_eq!(row["status"], "active");
    assert_eq!(row["source"], "feishu");
    for key in ["password_hash", "jwt_secret", "encryption_secret", "external_user_id"] {
        assert!(!row.as_object().unwrap().contains_key(key));
    }
    assert!(!String::from_utf8_lossy(&body).contains("synthetic-private-hash"));
}

#[tokio::test]
async fn directory_requires_authentication_and_real_super_admin() {
    let (app, jwt, regular, _db) = fixture().await;
    assert_eq!(
        call(app.clone(), None, "GET", false, None, "").await.0,
        StatusCode::UNAUTHORIZED
    );
    let token = jwt.sign(&regular, "ordinary-member").unwrap();
    let (status, body) = call(app, Some(token), "GET", false, None, "").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "FORBIDDEN");
    assert_eq!(body["error"], "Forbidden.");
}

#[tokio::test]
async fn directory_admin_get_is_empty_before_first_run() {
    let (app, jwt, _, _db) = fixture().await;
    let token = jwt.sign("system_default_user", "admin").unwrap();
    let (status, body) = call(app, Some(token), "GET", false, None, "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["success"], true);
    assert!(body["data"].is_null());
}

#[tokio::test]
async fn directory_post_requires_csrf_even_without_global_csrf_layer() {
    let (app, jwt, _, _db) = fixture().await;
    let token = jwt.sign("system_default_user", "admin").unwrap();
    let (status, body) = call(app.clone(), Some(token.clone()), "POST", false, None, "{}").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "CSRF_INVALID");
    let (status, body) = call(app, Some(token), "POST", true, None, "{}").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["code"], "FEISHU_DIRECTORY_CONFIG_UNAVAILABLE");
}

#[tokio::test]
async fn directory_rejects_act_as_and_strategy_overrides() {
    let (app, jwt, regular, _db) = fixture().await;
    let token = jwt.sign("system_default_user", "admin").unwrap();
    let (status, body) = call(app.clone(), Some(token.clone()), "POST", true, Some(&regular), "{}").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "BAD_REQUEST");
    assert_eq!(body["error"], "Admin endpoints do not accept act-as");
    let (status, _) = call(app, Some(token), "POST", true, None, r#"{"signup_policy":"open"}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
