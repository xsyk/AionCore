//! Exercise sharing through the production middleware tree, including legacy
//! deployments that disabled the old global CSRF layer.
mod common;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::{body_json, json_with_token};
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn share_is_live_read_only_revocable_and_keeps_private_response_headers() {
    let database = aionui_db::init_database_memory().await.unwrap();
    let config = aionui_app::AppConfig {
        disable_csrf: true,
        ..Default::default()
    };
    let services = aionui_app::AppServices::from_config(database, &config).await.unwrap();
    let app = aionui_app::create_router(&services).await.unwrap();
    let hash = aionui_auth::hash_password("TestP@ss1234").unwrap();
    services
        .user_repo
        .set_system_user_credentials("admin", &hash)
        .await
        .unwrap();
    let login = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"username":"admin","password":"TestP@ss1234"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let access = body_json(login).await["token"].as_str().unwrap().to_owned();
    let bootstrap = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/auth/feishu/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let csrf = bootstrap
        .headers()
        .get_all("set-cookie")
        .iter()
        .find_map(|value| {
            value
                .to_str()
                .ok()?
                .strip_prefix("aionui-csrf-token=")
                .map(|value| value.split(';').next().unwrap().to_owned())
        })
        .unwrap();
    sqlx::query("INSERT INTO conversations(id,user_id,name,type,extra,created_at,updated_at) VALUES ('shared-http','system_default_user','Shared HTTP','acp','{}',1,1)")
        .execute(services.database.pool()).await.unwrap();
    let route = "/api/conversations/shared-http/share";
    let missing_csrf = Request::builder()
        .method("PUT")
        .uri(route)
        .header("authorization", format!("Bearer {access}"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"enabled":true}"#))
        .unwrap();
    let denied = app.clone().oneshot(missing_csrf).await.unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(denied).await["code"], "CSRF_INVALID");
    let enabled = app
        .clone()
        .oneshot(json_with_token("PUT", route, json!({"enabled":true}), &access, &csrf))
        .await
        .unwrap();
    assert_eq!(enabled.status(), StatusCode::OK);
    let token = body_json(enabled).await["data"]["token"].as_str().unwrap().to_owned();
    sqlx::query("INSERT INTO messages(id,conversation_id,type,content,position,status,hidden,created_at) VALUES ('first','shared-http','text','{\"content\":\"Hello\"}','left','finish',0,1)")
        .execute(services.database.pool()).await.unwrap();
    let public = || {
        Request::builder()
            .uri("/api/public/shares/current/messages")
            .header("x-share-token", &token)
            .body(Body::empty())
            .unwrap()
    };
    let response = app.clone().oneshot(public()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["referrer-policy"], "no-referrer");
    assert_eq!(response.headers()["cache-control"], "private, no-store");
    assert_eq!(body_json(response).await["data"]["items"].as_array().unwrap().len(), 1);
    sqlx::query("INSERT INTO messages(id,conversation_id,type,content,position,status,hidden,created_at) VALUES ('later','shared-http','text','{\"content\":\"New message\"}','right','finish',0,2)")
        .execute(services.database.pool()).await.unwrap();
    let response = app.clone().oneshot(public()).await.unwrap();
    assert_eq!(body_json(response).await["data"]["items"].as_array().unwrap().len(), 2);
    let disabled = app
        .clone()
        .oneshot(json_with_token("PUT", route, json!({"enabled":false}), &access, &csrf))
        .await
        .unwrap();
    assert_eq!(disabled.status(), StatusCode::OK);
    assert_eq!(
        app.clone().oneshot(public()).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
    let restored = app
        .clone()
        .oneshot(json_with_token("PUT", route, json!({"enabled":true}), &access, &csrf))
        .await
        .unwrap();
    assert_eq!(body_json(restored).await["data"]["token"], token);
    assert_eq!(app.clone().oneshot(public()).await.unwrap().status(), StatusCode::OK);
    for path in [
        "/api/conversations/shared-http/messages",
        "/api/conversations/shared-http/confirmations",
    ] {
        let request = Request::builder()
            .method("POST")
            .uri(path)
            .header("x-share-token", &token)
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        assert_ne!(app.clone().oneshot(request).await.unwrap().status(), StatusCode::OK);
    }
    sqlx::query("DELETE FROM conversations WHERE id='shared-http'")
        .execute(services.database.pool())
        .await
        .unwrap();
    assert_eq!(app.oneshot(public()).await.unwrap().status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn sharing_and_binding_management_require_a_user_session() {
    let (app, _services) = common::build_app().await;
    for path in [
        "/api/conversations/unknown/share",
        "/api/conversations/unknown/feishu-binding",
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
