//! The image generation model is one setting for the whole server: everyone
//! reads it, only the administrator changes it.
//!
//! Through the real auth middleware this covers that the route needs a login,
//! that a change needs the CSRF token, that ordinary users read the setting but
//! cannot change it, and that the administrator, also while acting as another
//! user, can. The test server has no image generation script installed, so the
//! setting reads as unsupported and can only be saved switched off. The server
//! is built that way explicitly: `AIONUI_IMAGE_GEN_MCP_SCRIPT`, which a real
//! server reads its script from, does not matter to these tests.

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use aionui_app::{AppConfig, AppServices, create_router, derive_encryption_key};
use aionui_db::{SqliteGlobalSettingRepository, SqliteProviderRepository};
use aionui_system::ImageGenerationService;
use common::{body_json, get_request, get_with_token, json_with_token, setup_and_login};

const PASSWORD: &str = "StrongP@ss1";
const URI: &str = "/api/settings/image-generation";

/// An app whose server has no image generation script, whatever the environment
/// the tests run in says. `AppServices::from_config` takes the script from
/// `AIONUI_IMAGE_GEN_MCP_SCRIPT`, so the service is replaced before the router
/// is built.
async fn build_app_without_script() -> (Router, AppServices) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let services = AppServices::from_config(db, &AppConfig::default()).await.unwrap();
    let pool = services.database.pool().clone();
    let image_generation = ImageGenerationService::new(
        Arc::new(SqliteGlobalSettingRepository::new(pool.clone())),
        Arc::new(SqliteProviderRepository::new(pool)),
        derive_encryption_key(&services.encryption_secret_raw),
        None,
    );
    let services = services.with_image_generation_service(image_generation);
    let router = create_router(&services).await.expect("build router");
    (router, services)
}

struct Session {
    token: String,
    csrf: String,
}

/// An app with the administrator and one ordinary user (alice) logged in.
struct Harness {
    app: Router,
    services: AppServices,
    admin: Session,
    alice: Session,
}

impl Harness {
    async fn new() -> Self {
        let (mut app, services) = build_app_without_script().await;
        let (token, csrf) = setup_and_login(&mut app, &services, "admin", PASSWORD).await;
        let admin = Session { token, csrf };
        let (token, csrf) = setup_and_login(&mut app, &services, "alice", PASSWORD).await;
        let alice = Session { token, csrf };
        Self {
            app,
            services,
            admin,
            alice,
        }
    }

    async fn send(&self, req: Request<Body>) -> (StatusCode, Value) {
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    async fn alice_id(&self) -> String {
        self.services
            .user_repo
            .find_by_username("alice")
            .await
            .unwrap()
            .expect("alice exists")
            .id
    }

    /// What `session` reads from `GET /api/settings/image-generation`.
    async fn read(&self, session: &Session) -> Value {
        let (status, json) = self.send(get_with_token(URI, &session.token)).await;
        assert_eq!(status, StatusCode::OK, "{json}");
        json["data"].clone()
    }
}

fn put(session: &Session, body: Value) -> Request<Body> {
    json_with_token("PUT", URI, body, &session.token, &session.csrf)
}

fn acting_as(mut req: Request<Body>, user_id: &str) -> Request<Body> {
    req.headers_mut()
        .insert(aionui_auth::ACT_AS_HEADER, user_id.parse().unwrap());
    req
}

fn off() -> Value {
    json!({"provider_id": null, "model": null, "enabled": false, "supported": false})
}

#[tokio::test]
async fn the_image_generation_setting_needs_a_login() {
    let h = Harness::new().await;

    let (status, json) = h.send(get_request(URI)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{json}");
    assert_eq!(json["code"], "UNAUTHORIZED");

    // A valid CSRF pair but no bearer token.
    let mut req = put(&h.admin, json!({"provider_id": null, "model": null, "enabled": false}));
    req.headers_mut().remove("authorization");
    let (status, json) = h.send(req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{json}");
    assert_eq!(json["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn changing_the_image_generation_setting_needs_the_csrf_token() {
    let h = Harness::new().await;

    // The administrator's own request, minus the CSRF header and cookie.
    let mut req = put(&h.admin, json!({"provider_id": "p", "model": "m", "enabled": false}));
    req.headers_mut().remove("x-csrf-token");
    req.headers_mut().remove("cookie");
    let (status, json) = h.send(req).await;

    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    assert_eq!(json["code"], "CSRF_INVALID");
    assert_eq!(h.read(&h.admin).await, off(), "nothing was stored");
}

#[tokio::test]
async fn every_user_reads_the_setting_and_the_default_is_off_and_unsupported() {
    let h = Harness::new().await;

    assert_eq!(h.read(&h.admin).await, off());
    assert_eq!(h.read(&h.alice).await, off());
}

#[tokio::test]
async fn the_administrators_change_is_what_every_user_reads_afterwards() {
    let h = Harness::new().await;

    let (status, json) = h
        .send(put(
            &h.admin,
            json!({"provider_id": "prov_x", "model": "gpt-image-1", "enabled": false}),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["success"], true);

    let expected = json!({"provider_id": "prov_x", "model": "gpt-image-1", "enabled": false, "supported": false});
    assert_eq!(json["data"], expected, "the save answers with the stored setting");
    assert_eq!(h.read(&h.admin).await, expected);
    assert_eq!(h.read(&h.alice).await, expected);
}

#[tokio::test]
async fn ordinary_users_cannot_change_the_setting() {
    let h = Harness::new().await;

    for body in [
        json!({"provider_id": "prov_x", "model": "gpt-image-1", "enabled": false}),
        // The refusal comes before the body is looked at.
        json!({}),
    ] {
        let (status, json) = h.send(put(&h.alice, body.clone())).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "PUT {body}: {json}");
        assert_eq!(json["code"], "FORBIDDEN", "PUT {body}");
    }

    assert_eq!(h.read(&h.admin).await, off(), "nothing the user tried got through");
}

#[tokio::test]
async fn the_administrator_acting_as_a_user_can_change_the_setting() {
    let h = Harness::new().await;
    let alice = h.alice_id().await;

    let req = put(
        &h.admin,
        json!({"provider_id": "prov_x", "model": "m", "enabled": false}),
    );
    let (status, json) = h.send(acting_as(req, &alice)).await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(h.read(&h.alice).await["provider_id"], "prov_x");
}

#[tokio::test]
async fn switching_it_on_is_refused_on_a_server_without_the_image_generation_script() {
    let h = Harness::new().await;
    let (status, json) = h
        .send(put(
            &h.admin,
            json!({"provider_id": "prov_x", "model": "gpt-image-1", "enabled": true}),
        ))
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(json["code"], "BAD_REQUEST");
    assert_eq!(h.read(&h.alice).await, off());
}
