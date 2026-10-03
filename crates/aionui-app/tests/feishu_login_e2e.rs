//! End-to-end Feishu OAuth login against a wiremock Feishu.
//!
//! Runs the full router in webui identity mode (CSRF disabled so requests can
//! focus on the OAuth flow) against an in-memory database.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;
use wiremock::matchers::{body_partial_json, header as has_header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use aionui_app::{AppConfig, AppServices};

const ADMIN_PW: &str = "AdminP@ss123";

struct Ctx {
    app: axum::Router,
    _services: AppServices,
    feishu: MockServer,
    admin: String,
}

struct Resp {
    status: StatusCode,
    location: String,
    cookies: Vec<String>,
    json: serde_json::Value,
}

async fn call(
    app: &axum::Router,
    verb: &str,
    uri: &str,
    token: Option<&str>,
    cookie: Option<&str>,
    body: Option<serde_json::Value>,
) -> Resp {
    let mut builder = Request::builder().method(verb).uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    let req = match body {
        Some(json) => builder
            .header("content-type", "application/json")
            .body(Body::from(json.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let location = resp
        .headers()
        .get(header::LOCATION)
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let cookies = resp
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap().to_owned())
        .collect();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    Resp {
        status,
        location,
        cookies,
        json: serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    }
}

fn cookie_value(cookies: &[String], name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    cookies.iter().find_map(|c| {
        c.strip_prefix(&prefix)
            .map(|rest| rest.split(';').next().unwrap().to_owned())
    })
}

async fn setup() -> Ctx {
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
    let login = call(
        &app,
        "POST",
        "/login",
        None,
        None,
        Some(serde_json::json!({"username": "admin", "password": ADMIN_PW})),
    )
    .await;
    let admin = login.json["token"].as_str().expect("admin token").to_owned();
    Ctx {
        app,
        _services: services,
        feishu: MockServer::start().await,
        admin,
    }
}

async fn configure(ctx: &Ctx) -> Resp {
    call(
        &ctx.app,
        "PUT",
        "/api/admin/feishu-login",
        Some(&ctx.admin),
        None,
        Some(serde_json::json!({
            "enabled": true,
            "app_id": "cli_test",
            "app_secret": "sec_test",
            "public_base_url": "https://aidi.example.com",
            "api_base": ctx.feishu.uri(),
            "accounts_base": ctx.feishu.uri(),
        })),
    )
    .await
}

/// The mock Feishu answers authorization `code` with this identity.
async fn mock_feishu(ctx: &Ctx, code: &str, union_id: &str, name: &str, tenant: &str) {
    let token = format!("u-token-{code}");
    Mock::given(method("POST"))
        .and(path("/open-apis/authen/v2/oauth/token"))
        .and(body_partial_json(
            serde_json::json!({"code": code, "client_id": "cli_test", "client_secret": "sec_test"}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"code": 0, "access_token": token, "expires_in": 7200})),
        )
        .mount(&ctx.feishu)
        .await;
    Mock::given(method("GET"))
        .and(path("/open-apis/authen/v1/user_info"))
        .and(has_header("authorization", format!("Bearer {token}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "msg": "success",
            "data": {
                "union_id": union_id,
                "open_id": "ou_x",
                "name": name,
                "en_name": "",
                "avatar_url": "https://img.example.com/a.png",
                "enterprise_email": format!("{union_id}@corp.example.com"),
                "tenant_key": tenant
            }
        })))
        .mount(&ctx.feishu)
        .await;
}

/// Run start + callback the way a browser would; returns the callback response.
async fn feishu_login(ctx: &Ctx, code: &str) -> Resp {
    let start = call(&ctx.app, "GET", "/api/auth/feishu/start", None, None, None).await;
    assert_eq!(start.status, StatusCode::FOUND);
    let state = cookie_value(&start.cookies, "aionui-feishu-state").expect("state cookie");
    call(
        &ctx.app,
        "GET",
        &format!("/api/auth/feishu/callback?code={code}&state={state}"),
        None,
        Some(&format!("aionui-feishu-state={state}")),
        None,
    )
    .await
}

async fn me(ctx: &Ctx, callback: &Resp) -> serde_json::Value {
    let token = cookie_value(&callback.cookies, "aionui-session").expect("session cookie");
    call(&ctx.app, "GET", "/api/auth/user", Some(&token), None, None)
        .await
        .json["user"]
        .clone()
}

#[tokio::test]
async fn disabled_by_default() {
    let ctx = setup().await;
    let status = call(&ctx.app, "GET", "/api/auth/feishu/status", None, None, None).await;
    assert_eq!(status.status, StatusCode::OK);
    assert_eq!(status.json["data"]["enabled"], false);
    let start = call(&ctx.app, "GET", "/api/auth/feishu/start", None, None, None).await;
    assert_eq!(start.status, StatusCode::FOUND);
    assert_eq!(start.location, "/#/login?feishu_error=disabled");
}

#[tokio::test]
async fn admin_config_hides_secret_and_requires_super_admin() {
    let ctx = setup().await;
    let put = configure(&ctx).await;
    assert_eq!(put.status, StatusCode::OK, "{}", put.json);
    let get = call(&ctx.app, "GET", "/api/admin/feishu-login", Some(&ctx.admin), None, None).await;
    assert_eq!(get.json["data"]["app_secret_set"], true);
    assert!(!get.json.to_string().contains("sec_test"), "secret must not leak");
    assert_eq!(
        get.json["data"]["callback_url"],
        "https://aidi.example.com/api/auth/feishu/callback"
    );
    // enabling without a site URL is rejected
    let bad = call(
        &ctx.app,
        "PUT",
        "/api/admin/feishu-login",
        Some(&ctx.admin),
        None,
        Some(serde_json::json!({"enabled": true, "app_id": "x", "public_base_url": ""})),
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    // a normal user gets 403
    let created = call(
        &ctx.app,
        "POST",
        "/api/admin/users",
        Some(&ctx.admin),
        None,
        Some(serde_json::json!({"username": "bob", "password": "BobP@ss1234"})),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json);
    let bob = call(
        &ctx.app,
        "POST",
        "/login",
        None,
        None,
        Some(serde_json::json!({"username": "bob", "password": "BobP@ss1234"})),
    )
    .await;
    let bob = bob.json["token"].as_str().unwrap().to_owned();
    let denied = call(&ctx.app, "GET", "/api/admin/feishu-login", Some(&bob), None, None).await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    let users = call(&ctx.app, "GET", "/api/admin/users", Some(&ctx.admin), None, None).await;
    assert!(
        users.json["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|u| u["source"] == "password")
    );
}

#[tokio::test]
async fn start_redirects_to_feishu_with_state() {
    let ctx = setup().await;
    configure(&ctx).await;
    let status = call(&ctx.app, "GET", "/api/auth/feishu/status", None, None, None).await;
    assert_eq!(status.json["data"]["enabled"], true);
    let start = call(&ctx.app, "GET", "/api/auth/feishu/start", None, None, None).await;
    let state = cookie_value(&start.cookies, "aionui-feishu-state").unwrap();
    assert!(
        start.location.starts_with(&format!(
            "{}/open-apis/authen/v1/authorize?client_id=cli_test",
            ctx.feishu.uri()
        )),
        "{}",
        start.location
    );
    assert!(start.location.contains(&format!("state={state}")));
    assert!(
        start
            .location
            .contains("redirect_uri=https%3A%2F%2Faidi.example.com%2Fapi%2Fauth%2Ffeishu%2Fcallback")
    );
    let raw = start
        .cookies
        .iter()
        .find(|c| c.starts_with("aionui-feishu-state="))
        .unwrap();
    assert!(raw.contains("HttpOnly") && raw.contains("SameSite=Lax") && raw.contains("Path=/api/auth/feishu"));
}

#[tokio::test]
async fn first_login_creates_account_and_second_reuses_it() {
    let ctx = setup().await;
    configure(&ctx).await;
    mock_feishu(&ctx, "c1", "on_zhang", "张三", "tenant_a").await;
    let cb = feishu_login(&ctx, "c1").await;
    assert_eq!(cb.status, StatusCode::FOUND);
    assert_eq!(cb.location, "/#/guid");
    assert!(cookie_value(&cb.cookies, "aionui-refresh").is_some());
    let user = me(&ctx, &cb).await;
    assert_eq!(user["username"], "张三", "{user}");
    assert_eq!(user["is_super_admin"], false);
    let id = user["id"].as_str().unwrap().to_owned();
    // tenant got locked by the first login
    let cfg = call(&ctx.app, "GET", "/api/admin/feishu-login", Some(&ctx.admin), None, None).await;
    assert_eq!(cfg.json["data"]["tenant_key"], "tenant_a");
    // listed with the feishu source
    let users = call(&ctx.app, "GET", "/api/admin/users", Some(&ctx.admin), None, None).await;
    let row = users.json["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["id"] == id.as_str())
        .cloned()
        .expect("listed");
    assert_eq!(row["source"], "feishu");
    // second login reuses the account
    let cb2 = feishu_login(&ctx, "c1").await;
    assert_eq!(me(&ctx, &cb2).await["id"], id.as_str());
}

#[tokio::test]
async fn rejects_bad_state_missing_code_and_foreign_tenant() {
    let ctx = setup().await;
    configure(&ctx).await;
    let forged = call(
        &ctx.app,
        "GET",
        "/api/auth/feishu/callback?code=c1&state=forged",
        None,
        Some("aionui-feishu-state=real"),
        None,
    )
    .await;
    assert_eq!(forged.location, "/#/login?feishu_error=state");
    let no_cookie = call(
        &ctx.app,
        "GET",
        "/api/auth/feishu/callback?code=c1&state=x",
        None,
        None,
        None,
    )
    .await;
    assert_eq!(no_cookie.location, "/#/login?feishu_error=state");
    let cancelled = call(
        &ctx.app,
        "GET",
        "/api/auth/feishu/callback?state=s",
        None,
        Some("aionui-feishu-state=s"),
        None,
    )
    .await;
    assert_eq!(cancelled.location, "/#/login?feishu_error=cancelled");
    mock_feishu(&ctx, "c1", "on_a", "A", "tenant_a").await;
    mock_feishu(&ctx, "c2", "on_b", "B", "tenant_other").await;
    assert_eq!(feishu_login(&ctx, "c1").await.location, "/#/guid");
    assert_eq!(feishu_login(&ctx, "c2").await.location, "/#/login?feishu_error=tenant");
}

#[tokio::test]
async fn disabled_and_deleted_accounts() {
    let ctx = setup().await;
    configure(&ctx).await;
    mock_feishu(&ctx, "c1", "on_li", "李四", "t").await;
    let cb = feishu_login(&ctx, "c1").await;
    let id = me(&ctx, &cb).await["id"].as_str().unwrap().to_owned();
    let disabled = call(
        &ctx.app,
        "POST",
        &format!("/api/admin/users/{id}/disable"),
        Some(&ctx.admin),
        None,
        None,
    )
    .await;
    assert_eq!(disabled.status, StatusCode::OK);
    assert_eq!(
        feishu_login(&ctx, "c1").await.location,
        "/#/login?feishu_error=account_disabled"
    );
    let deleted = call(
        &ctx.app,
        "DELETE",
        &format!("/api/admin/users/{id}"),
        Some(&ctx.admin),
        None,
        None,
    )
    .await;
    assert_eq!(deleted.status, StatusCode::OK);
    let cb = feishu_login(&ctx, "c1").await;
    assert_eq!(cb.location, "/#/guid");
    assert_ne!(
        me(&ctx, &cb).await["id"],
        id.as_str(),
        "a deleted user comes back as a new account"
    );
}

#[tokio::test]
async fn namesakes_get_distinct_usernames_and_upstream_errors_map() {
    let ctx = setup().await;
    configure(&ctx).await;
    mock_feishu(&ctx, "c1", "on_w0001", "王五", "t").await;
    mock_feishu(&ctx, "c2", "on_w0002", "王五", "t").await;
    let first = feishu_login(&ctx, "c1").await;
    assert_eq!(me(&ctx, &first).await["username"], "王五");
    let second = feishu_login(&ctx, "c2").await;
    assert_eq!(me(&ctx, &second).await["username"], "王五-0002");
    // no mock for this code → token endpoint 404 → upstream
    assert_eq!(
        feishu_login(&ctx, "nope").await.location,
        "/#/login?feishu_error=upstream"
    );
}
