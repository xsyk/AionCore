//! Agent settings are shared by every user: the administrator manages them and
//! everybody uses them.
//!
//! Through the real auth middleware this covers that custom agents belong to
//! nobody (so every user sees them), that every route that changes an agent or
//! reads its overrides (environment variables, often API keys) is the
//! administrator's alone, also while the administrator acts as another user, and
//! that no environment value reaches an ordinary user through the agent list.
//!
//! `AIONUI_BYPASS_PROBE` lets a custom agent be saved without a real ACP CLI. It
//! is process-wide, so every test that creates an agent holds a `ProbeBypass`
//! for its whole body.

mod common;

use std::sync::OnceLock;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tokio::sync::{Mutex, MutexGuard};
use tower::ServiceExt;

use aionui_app::AppServices;
use common::{body_json, build_app, delete_with_token, get_with_token, json_with_token, setup_and_login};

const PASSWORD: &str = "StrongP@ss1";

static ENV_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();

/// Holds the process-wide lock and keeps the probe bypass switched on until dropped.
struct ProbeBypass {
    _guard: MutexGuard<'static, ()>,
}

impl ProbeBypass {
    async fn on() -> Self {
        let guard = ENV_MUTEX.get_or_init(|| Mutex::new(())).lock().await;
        // SAFETY: the variable is only touched while holding ENV_MUTEX.
        unsafe {
            std::env::set_var("AIONUI_BYPASS_PROBE", "1");
        }
        Self { _guard: guard }
    }
}

impl Drop for ProbeBypass {
    fn drop(&mut self) {
        // SAFETY: the lock in `_guard` is still held; it is released after this runs.
        unsafe {
            std::env::remove_var("AIONUI_BYPASS_PROBE");
        }
    }
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
        let (mut app, services) = build_app().await;
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

    async fn user_id(&self, username: &str) -> String {
        self.services
            .user_repo
            .find_by_username(username)
            .await
            .unwrap()
            .expect("user exists")
            .id
    }

    /// The administrator saves a custom agent running `sh` and returns its id.
    async fn admin_creates_agent(&self, name: &str, env: Value) -> String {
        let req = request(
            &self.admin,
            "POST",
            "/api/agents/custom",
            Some(json!({"name": name, "command": "sh", "args": ["--acp"], "env": env})),
        );
        let (status, json) = self.send(req).await;
        assert_eq!(status, StatusCode::OK, "create failed: {json}");
        json["data"]["id"].as_str().expect("id in response").to_owned()
    }

    /// The row for `id` in `GET /api/agents/management` as `session` sees it.
    async fn management_row(&self, session: &Session, id: &str) -> Option<Value> {
        let (status, json) = self.send(request(session, "GET", "/api/agents/management", None)).await;
        assert_eq!(status, StatusCode::OK, "{json}");
        json["data"]
            .as_array()
            .expect("management list is an array")
            .iter()
            .find(|row| row["id"] == id)
            .cloned()
    }

    /// Who owns the stored agent row (`None`: it belongs to nobody).
    async fn owner_of(&self, agent_id: &str) -> Option<String> {
        sqlx::query_scalar::<_, Option<String>>("SELECT user_id FROM agent_metadata WHERE agent_id = ?")
            .bind(agent_id)
            .fetch_one(self.services.database.pool())
            .await
            .expect("agent row exists")
    }
}

fn request(session: &Session, method: &str, uri: &str, body: Option<Value>) -> Request<Body> {
    match (method, body) {
        ("GET", _) => get_with_token(uri, &session.token),
        ("DELETE", None) => delete_with_token(uri, &session.token, &session.csrf),
        (_, Some(body)) => json_with_token(method, uri, body, &session.token, &session.csrf),
        (_, None) => panic!("{method} {uri} needs a body"),
    }
}

fn acting_as(mut req: Request<Body>, user_id: &str) -> Request<Body> {
    req.headers_mut()
        .insert(aionui_auth::ACT_AS_HEADER, user_id.parse().unwrap());
    req
}

/// Every route that changes an agent or reads its overrides, aimed at agent `id`.
fn administrator_only_requests(id: &str) -> Vec<(&'static str, String, Option<Value>)> {
    vec![
        (
            "POST",
            "/api/agents/custom".to_owned(),
            Some(json!({"name": "Mine", "command": "sh"})),
        ),
        (
            "PUT",
            format!("/api/agents/custom/{id}"),
            Some(json!({"name": "Hijacked", "command": "sh"})),
        ),
        ("DELETE", format!("/api/agents/custom/{id}"), None),
        (
            "PATCH",
            format!("/api/agents/{id}/enabled"),
            Some(json!({"enabled": false})),
        ),
        ("GET", format!("/api/agents/{id}/overrides"), None),
        (
            "PUT",
            format!("/api/agents/{id}/overrides"),
            Some(json!({"command_override": "true", "env_override": [{"name": "K", "value": "planted"}]})),
        ),
        (
            "POST",
            "/api/agents/custom/try-connect".to_owned(),
            Some(json!({"command": "sh"})),
        ),
    ]
}

#[tokio::test]
async fn agent_routes_need_a_login() {
    let h = Harness::new().await;
    let mut requests = administrator_only_requests("any-agent");
    requests.push(("GET", "/api/agents/management".to_owned(), None));

    for (method, uri, body) in requests {
        // A valid CSRF pair but no bearer token.
        let mut req = request(&h.admin, method, &uri, body);
        req.headers_mut().remove("authorization");
        let (status, json) = h.send(req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri}: {json}");
        assert_eq!(json["code"], "UNAUTHORIZED", "{method} {uri}");
    }
}

#[tokio::test]
async fn changing_an_agent_needs_the_csrf_token() {
    let _probe = ProbeBypass::on().await;
    let h = Harness::new().await;
    let id = h.admin_creates_agent("Shared Agent", json!([])).await;

    for (method, uri, body) in administrator_only_requests(&id) {
        if method == "GET" {
            continue;
        }
        // The administrator's own request, minus the CSRF header and cookie.
        let mut req = request(&h.admin, method, &uri, body);
        req.headers_mut().remove("x-csrf-token");
        req.headers_mut().remove("cookie");
        let (status, json) = h.send(req).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}: {json}");
        assert_eq!(json["code"], "CSRF_INVALID", "{method} {uri}");
    }

    // None of them got through.
    let row = h.management_row(&h.admin, &id).await.expect("agent still there");
    assert_eq!(row["name"], "Shared Agent");
    assert_eq!(row["enabled"], true);
}

#[tokio::test]
async fn a_custom_agent_the_administrator_creates_is_visible_to_every_user() {
    let _probe = ProbeBypass::on().await;
    let h = Harness::new().await;

    let id = h.admin_creates_agent("Shared Agent", json!([])).await;

    for (who, session) in [("the administrator", &h.admin), ("an ordinary user", &h.alice)] {
        let row = h
            .management_row(session, &id)
            .await
            .unwrap_or_else(|| panic!("{who} must see the custom agent"));
        assert_eq!(row["name"], "Shared Agent", "{who}");
        assert_eq!(row["agent_source"], "custom", "{who}");
    }
    assert_eq!(h.owner_of(&id).await, None, "a shared agent belongs to nobody");
}

#[tokio::test]
async fn the_administrator_acting_as_a_user_manages_agents_that_belong_to_nobody() {
    let _probe = ProbeBypass::on().await;
    let h = Harness::new().await;
    let alice = h.user_id("alice").await;

    // Created while acting as alice, it is still nobody's: alice does not become its owner.
    let create = request(
        &h.admin,
        "POST",
        "/api/agents/custom",
        Some(json!({"name": "Made as alice", "command": "sh"})),
    );
    let (status, json) = h.send(acting_as(create, &alice)).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let id = json["data"]["id"].as_str().unwrap().to_owned();
    assert_eq!(h.owner_of(&id).await, None);
    assert!(
        h.management_row(&h.admin, &id).await.is_some(),
        "the administrator sees it when not acting as alice"
    );
    assert!(h.management_row(&h.alice, &id).await.is_some(), "alice sees it");

    // Edit, toggle, read overrides, test connection, delete: all still allowed.
    let rename = request(
        &h.admin,
        "PUT",
        &format!("/api/agents/custom/{id}"),
        Some(json!({"name": "Renamed as alice", "command": "sh"})),
    );
    let (status, json) = h.send(acting_as(rename, &alice)).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["data"]["name"], "Renamed as alice");
    assert_eq!(h.owner_of(&id).await, None);

    let disable = request(
        &h.admin,
        "PATCH",
        &format!("/api/agents/{id}/enabled"),
        Some(json!({"enabled": false})),
    );
    let (status, json) = h.send(acting_as(disable, &alice)).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["data"]["enabled"], false);

    let overrides = request(&h.admin, "GET", &format!("/api/agents/{id}/overrides"), None);
    let (status, json) = h.send(acting_as(overrides, &alice)).await;
    assert_eq!(status, StatusCode::OK, "{json}");

    let try_connect = request(
        &h.admin,
        "POST",
        "/api/agents/custom/try-connect",
        Some(json!({"command": "/nonexistent/path/to/agent"})),
    );
    let (status, json) = h.send(acting_as(try_connect, &alice)).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["data"]["step"], "fail_cli");

    let delete = request(&h.admin, "DELETE", &format!("/api/agents/custom/{id}"), None);
    let (status, json) = h.send(acting_as(delete, &alice)).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["data"]["deleted"], true);
    assert!(
        h.management_row(&h.alice, &id).await.is_none(),
        "it is gone for everyone"
    );
}

#[tokio::test]
async fn ordinary_users_cannot_change_agents_or_read_their_overrides() {
    let _probe = ProbeBypass::on().await;
    let h = Harness::new().await;
    let id = h.admin_creates_agent("Shared Agent", json!([])).await;

    for (method, uri, body) in administrator_only_requests(&id) {
        let (status, json) = h.send(request(&h.alice, method, &uri, body)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}: {json}");
        assert_eq!(json["code"], "FORBIDDEN", "{method} {uri}");
    }

    // Nothing the user tried got through.
    let row = h.management_row(&h.admin, &id).await.expect("agent still there");
    assert_eq!(row["name"], "Shared Agent");
    assert_eq!(row["enabled"], true);
    let custom_agents = {
        let (_, json) = h.send(request(&h.admin, "GET", "/api/agents/management", None)).await;
        json["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["agent_source"] == "custom")
            .count()
    };
    assert_eq!(custom_agents, 1, "the user's POST created nothing");
    let (status, json) = h
        .send(request(&h.admin, "GET", &format!("/api/agents/{id}/overrides"), None))
        .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["data"]["env_override"], json!([]), "the user's PUT stored nothing");
    assert_eq!(json["data"].get("command_override"), None);
}

#[tokio::test]
async fn ordinary_users_are_refused_before_validation_and_lookup() {
    let h = Harness::new().await;
    let missing = "no-such-agent";

    // What each request earns from the administrator (a validation or lookup
    // error) is exactly what the refusal must keep an ordinary user from learning.
    let cases: Vec<(&str, String, Value, StatusCode, &str)> = vec![
        (
            "POST",
            "/api/agents/custom".to_owned(),
            json!({}),
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
        ),
        (
            "PUT",
            format!("/api/agents/custom/{missing}"),
            json!({}),
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
        ),
        (
            "PATCH",
            format!("/api/agents/{missing}/enabled"),
            json!({}),
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
        ),
        (
            "PUT",
            format!("/api/agents/{missing}/overrides"),
            json!({}),
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
        ),
        (
            "POST",
            "/api/agents/custom/try-connect".to_owned(),
            json!({}),
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
        ),
    ];
    for (method, uri, body, admin_status, admin_code) in cases {
        let (status, json) = h.send(request(&h.admin, method, &uri, Some(body.clone()))).await;
        assert_eq!(status, admin_status, "administrator {method} {uri}: {json}");
        assert_eq!(json["code"], admin_code, "administrator {method} {uri}");

        let (status, json) = h.send(request(&h.alice, method, &uri, Some(body))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "user {method} {uri}: {json}");
        assert_eq!(json["code"], "FORBIDDEN", "user {method} {uri}");
    }

    let lookups = [
        ("DELETE", format!("/api/agents/custom/{missing}")),
        ("GET", format!("/api/agents/{missing}/overrides")),
    ];
    for (method, uri) in lookups {
        let (status, json) = h.send(request(&h.admin, method, &uri, None)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "administrator {method} {uri}: {json}");
        assert_eq!(json["code"], "NOT_FOUND", "administrator {method} {uri}");

        let (status, json) = h.send(request(&h.alice, method, &uri, None)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "user {method} {uri}: {json}");
        assert_eq!(json["code"], "FORBIDDEN", "user {method} {uri}");
    }
}

#[tokio::test]
async fn health_checks_stay_open_to_ordinary_users() {
    let h = Harness::new().await;

    // Not refused: the request gets as far as looking the agent up.
    let (status, json) = h
        .send(request(
            &h.alice,
            "POST",
            "/api/agents/no-such-agent/health-check",
            Some(json!({})),
        ))
        .await;

    assert_eq!(status, StatusCode::NOT_FOUND, "{json}");
    assert_eq!(json["code"], "NOT_FOUND");
}

#[tokio::test]
async fn ordinary_users_never_see_environment_values_in_the_agent_list() {
    let _probe = ProbeBypass::on().await;
    let h = Harness::new().await;
    let id = h
        .admin_creates_agent(
            "Keyed Agent",
            json!([{"name": "SHARED_API_KEY", "value": "sk-custom-secret"}]),
        )
        .await;
    // An override written by the administrator on top of the agent's own env
    // (stored directly: the PUT route would also run a health check).
    h.services
        .agent_registry
        .repo_handle()
        .update_agent_overrides(
            &id,
            None,
            Some(r#"[{"name":"OVERRIDE_API_KEY","value":"sk-override-secret"}]"#),
        )
        .await
        .unwrap();

    // The secrets are really stored, and the administrator reads them where they are meant to be read.
    let (status, json) = h
        .send(request(&h.admin, "GET", &format!("/api/agents/{id}/overrides"), None))
        .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["data"]["env_override"][0]["value"], "sk-override-secret");

    // The administrator's list carries the agent's variables with their values: the
    // editor prefills from it, and saving what it shows must not wipe them.
    let row = h
        .management_row(&h.admin, &id)
        .await
        .expect("the administrator sees the agent");
    assert_eq!(
        row["env"],
        json!([{"name": "SHARED_API_KEY", "value": "sk-custom-secret"}]),
        "the administrator's list must carry the stored env: {row}"
    );

    // The shared agent is listed for an ordinary user too: the variable names, no value anywhere.
    let (status, listing) = h.send(request(&h.alice, "GET", "/api/agents/management", None)).await;
    assert_eq!(status, StatusCode::OK, "{listing}");
    let row = listing["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == id.as_str())
        .expect("the shared agent is listed for an ordinary user");
    assert_eq!(row["env_override_key_count"], 1, "which variables exist is not secret");
    assert_eq!(
        row["env"],
        json!([{"name": "SHARED_API_KEY", "value": ""}]),
        "an ordinary user sees which variables are set, with empty values: {row}"
    );
    let listing = listing.to_string();
    for secret in ["sk-custom-secret", "sk-override-secret"] {
        assert!(
            !listing.contains(secret),
            "{secret} leaked into the agent list: {listing}"
        );
    }
}

#[tokio::test]
async fn a_health_check_hides_environment_values_from_ordinary_users_too() {
    let _probe = ProbeBypass::on().await;
    let h = Harness::new().await;
    // A command that does not exist keeps the probe instant (the save is let through by the bypass).
    let create = request(
        &h.admin,
        "POST",
        "/api/agents/custom",
        Some(json!({
            "name": "Keyed Agent",
            "command": "/nonexistent/path/to/agent",
            "env": [{"name": "SHARED_API_KEY", "value": "sk-custom-secret"}]
        })),
    );
    let (status, json) = h.send(create).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let id = json["data"]["id"].as_str().expect("id in response").to_owned();
    let health_check = |session: &Session| {
        request(
            session,
            "POST",
            &format!("/api/agents/{id}/health-check"),
            Some(json!({})),
        )
    };

    // The check answers with the agent's management row: the administrator gets the values ...
    let (status, json) = h.send(health_check(&h.admin)).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(
        json["data"]["env"],
        json!([{"name": "SHARED_API_KEY", "value": "sk-custom-secret"}])
    );

    // ... an ordinary user gets the names and nothing else.
    let (status, json) = h.send(health_check(&h.alice)).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["data"]["env"], json!([{"name": "SHARED_API_KEY", "value": ""}]));
    assert!(
        !json.to_string().contains("sk-custom-secret"),
        "the secret leaked into the health check answer: {json}"
    );
}
