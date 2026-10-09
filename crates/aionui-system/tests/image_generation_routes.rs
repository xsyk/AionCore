//! Black-box tests for the shared image generation setting:
//! `GET`/`PUT /api/settings/image-generation` and the session MCP server the
//! backend builds from it.
//!
//! Tests exercise the HTTP layer through `tower::ServiceExt::oneshot`, without
//! authentication middleware: each request carries the `CurrentUser` (and
//! `RealUser`) that the middleware would have injected, built by `request_as`.
//! The "installed" image generation script is a real temp file, and Node is
//! located by an injected resolver so no test depends on the machine's runtime.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use aionui_realtime::BroadcastEventBus;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use aionui_api_types::{SessionMcpServer, SessionMcpTransport};
use aionui_auth::{CurrentUser, RealUser};
use aionui_common::constants::SUPER_ADMIN_USER_ID;
use aionui_common::encrypt_string;
use aionui_db::{
    CreateProviderParams, Database, IGlobalSettingRepository, IProviderRepository, SqliteClientPreferenceRepository,
    SqliteFeedbackDiagnosticsRepository, SqliteGlobalSettingRepository, SqliteProviderRepository,
    SqliteSettingsRepository, UpdateProviderParams, UserStatus, UserType, init_database_memory,
};
use aionui_runtime::ResolvedCommand;
use aionui_system::{
    ClientPrefService, FeedbackDiagnosticsService, IMAGE_GENERATION_MCP_NAME, IMAGE_GENERATION_SETTING_KEY,
    ImageGenerationService, ModelFetchService, ProtocolDetectionService, ProviderService, RuntimePrepareService,
    SettingsService, SystemRouterState, VersionCheckService, system_routes,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const KEY: [u8; 32] = [0x42; 32];
/// The administrator, the only account that may change the setting. Its user
/// row is seeded by `init_database_memory`.
const ADMIN_ID: &str = SUPER_ADMIN_USER_ID;
const USER_ID: &str = "user-1";
const URI: &str = "/api/settings/image-generation";
const PROVIDER_ID: &str = "prov-image";
const IMAGE_MODEL: &str = "gpt-image-1";
const PROVIDER_KEY: &str = "sk-image-secret";
const PROVIDER_BASE_URL: &str = "https://img.example.com/v1";

/// What the injected resolver reports for "node".
fn fake_node() -> ResolvedCommand {
    ResolvedCommand {
        program: PathBuf::from("/test/bin/node"),
        args_prefix: vec![OsString::from("--test-flag")],
        env: vec![(OsString::from("NODE_TEST_ENV"), OsString::from("1"))],
    }
}

/// The state a request is served with: the system services over `db` plus the
/// given image generation service.
fn state(db: &Database, image_generation_service: ImageGenerationService) -> SystemRouterState {
    let provider_repo = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
    let http_client = reqwest::Client::new();
    SystemRouterState {
        settings_service: SettingsService::new(Arc::new(SqliteSettingsRepository::new(db.pool().clone()))),
        client_pref_service: ClientPrefService::new(Arc::new(SqliteClientPreferenceRepository::new(db.pool().clone()))),
        provider_service: ProviderService::new(provider_repo.clone(), KEY),
        model_fetch_service: ModelFetchService::new(provider_repo, KEY, http_client.clone()),
        protocol_detection_service: ProtocolDetectionService::new(http_client.clone()),
        version_check_service: VersionCheckService::new(http_client, "0.1.0".to_owned()),
        runtime_prepare_service: RuntimePrepareService::new(Arc::new(BroadcastEventBus::new(16))),
        feedback_diagnostics_service: FeedbackDiagnosticsService::new(Arc::new(
            SqliteFeedbackDiagnosticsRepository::new(db.pool().clone()),
        )),
        image_generation_service,
    }
}

/// An image generation service over `db` whose Node lookup is `fake_node`.
fn image_service(db: &Database, script: Option<PathBuf>) -> ImageGenerationService {
    ImageGenerationService::new(
        Arc::new(SqliteGlobalSettingRepository::new(db.pool().clone())),
        Arc::new(SqliteProviderRepository::new(db.pool().clone())),
        KEY,
        script,
    )
    .with_node_resolver(|| async { Some(fake_node()) })
}

/// A database, the image generation service over it and, when the server has
/// the component installed, the script file that proves it.
struct Env {
    db: Database,
    service: ImageGenerationService,
    /// Keeps the script file alive for the whole test.
    script: Option<tempfile::NamedTempFile>,
}

impl Env {
    /// A server with the image generation script installed.
    async fn with_script() -> Self {
        let script = tempfile::Builder::new().suffix(".js").tempfile().unwrap();
        let path = script.path().to_path_buf();
        Self::build(Some(script), Some(path)).await
    }

    /// A server without the image generation component.
    async fn without_script() -> Self {
        Self::build(None, None).await
    }

    /// A server whose configured script path is `path`, whatever is (or is not) there.
    async fn with_script_path(path: PathBuf) -> Self {
        Self::build(None, Some(path)).await
    }

    async fn build(script: Option<tempfile::NamedTempFile>, script_path: Option<PathBuf>) -> Self {
        let db = init_database_memory().await.unwrap();
        sqlx::query(
            "INSERT INTO users (id, user_type, username, password_hash, status, session_generation, created_at, updated_at) \
             VALUES (?, 'local', ?, '', 'active', 0, 1, 1)",
        )
        .bind(USER_ID)
        .bind(USER_ID)
        .execute(db.pool())
        .await
        .unwrap();
        let service = image_service(&db, script_path);
        Self { db, service, script }
    }

    fn script_path(&self) -> PathBuf {
        self.script
            .as_ref()
            .expect("this server has a script")
            .path()
            .to_path_buf()
    }

    /// Send one request through a fresh router over this server's database.
    async fn send(&self, req: Request<Body>) -> axum::response::Response {
        system_routes(state(&self.db, self.service.clone()))
            .oneshot(req)
            .await
            .unwrap()
    }

    /// `GET` as `user_id`; expects 200 and returns the `data` object.
    async fn get_as(&self, user_id: &str) -> Value {
        let resp = self.send(request_as(Some(user_id), user_id, "GET", URI, None)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(json["success"], true);
        json["data"].clone()
    }

    /// `PUT` as the administrator; expects 200 and returns the `data` object.
    async fn put_ok(&self, body: Value) -> Value {
        let resp = self
            .send(request_as(Some(ADMIN_ID), ADMIN_ID, "PUT", URI, Some(body.clone())))
            .await;
        assert_eq!(resp.status(), StatusCode::OK, "PUT {body}");
        let json = body_json(resp).await;
        assert_eq!(json["success"], true);
        json["data"].clone()
    }

    /// `PUT` as the administrator and expect the 400 `BAD_REQUEST` of a refused
    /// setting; returns the error message.
    async fn put_rejected(&self, body: Value) -> String {
        let resp = self
            .send(request_as(Some(ADMIN_ID), ADMIN_ID, "PUT", URI, Some(body.clone())))
            .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "PUT {body}");
        let json = body_json(resp).await;
        assert_eq!(json["code"], "BAD_REQUEST", "PUT {body}");
        json["error"].as_str().unwrap_or_default().to_owned()
    }

    /// Switch image generation on for `provider_id`/`model` as the administrator.
    async fn enable(&self, provider_id: &str, model: &str) {
        self.put_ok(json!({"provider_id": provider_id, "model": model, "enabled": true}))
            .await;
    }

    /// Add an enabled OpenAI-style provider whose stored key is `PROVIDER_KEY`.
    async fn add_provider(&self, id: &str, models: &[&str]) {
        self.add_provider_with_key(id, models, PROVIDER_KEY).await;
    }

    async fn add_provider_with_key(&self, id: &str, models: &[&str], api_key: &str) {
        let encrypted = encrypt_string(api_key, &KEY).unwrap();
        self.insert_provider(id, models, &encrypted).await;
    }

    /// Insert a provider row whose `api_key_encrypted` is stored exactly as given.
    async fn insert_provider(&self, id: &str, models: &[&str], api_key_encrypted: &str) {
        let models = serde_json::to_string(models).unwrap();
        SqliteProviderRepository::new(self.db.pool().clone())
            .create(CreateProviderParams {
                id: Some(id),
                user_id: ADMIN_ID,
                platform: "openai",
                name: "Image provider",
                base_url: PROVIDER_BASE_URL,
                api_key_encrypted,
                models: &models,
                enabled: true,
                capabilities: "[]",
                context_limit: None,
                model_protocols: None,
                model_enabled: None,
                model_health: None,
                model_settings: "{}",
                bedrock_config: None,
                is_full_url: false,
            })
            .await
            .unwrap();
    }

    async fn delete_provider(&self, id: &str) {
        SqliteProviderRepository::new(self.db.pool().clone())
            .delete(id)
            .await
            .unwrap();
    }

    async fn set_provider_enabled(&self, id: &str, enabled: bool) {
        SqliteProviderRepository::new(self.db.pool().clone())
            .update(
                id,
                UpdateProviderParams {
                    enabled: Some(enabled),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }

    /// Write the raw stored value, bypassing the API (a hand-edited, migrated or
    /// damaged row).
    async fn store_raw(&self, value: &str) {
        SqliteGlobalSettingRepository::new(self.db.pool().clone())
            .set(IMAGE_GENERATION_SETTING_KEY, value)
            .await
            .unwrap();
    }

    async fn stored_raw(&self) -> Option<String> {
        SqliteGlobalSettingRepository::new(self.db.pool().clone())
            .get(IMAGE_GENERATION_SETTING_KEY)
            .await
            .unwrap()
    }
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn current_user(id: &str) -> CurrentUser {
    CurrentUser {
        id: id.to_owned(),
        username: id.to_owned(),
        user_type: UserType::Local,
        status: UserStatus::Active,
    }
}

/// Build a request the way the auth middleware leaves it: `current` is the
/// effective user and `real`, when `Some`, the authenticated caller behind it.
/// The two differ only while the administrator acts as another user. With
/// `real = None` only `CurrentUser` is injected, as a router that was built
/// without the middleware sees requests.
fn request_as(real: Option<&str>, current: &str, method: &str, uri: &str, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let mut req = match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    if let Some(real) = real {
        req.extensions_mut().insert(RealUser(current_user(real)));
    }
    req.extensions_mut().insert(current_user(current));
    req
}

/// The stdio launch of a session server: (command, args, env).
fn stdio(server: &SessionMcpServer) -> (&str, &[String], &HashMap<String, String>) {
    match &server.transport {
        SessionMcpTransport::Stdio { command, args, env } => (command.as_str(), args.as_slice(), env),
        other => panic!("the image generation server is a stdio server, got {other:?}"),
    }
}

// ===========================================================================
// GET /api/settings/image-generation
// ===========================================================================

#[tokio::test]
async fn get_reports_disabled_and_supported_when_nothing_was_ever_saved() {
    let env = Env::with_script().await;

    assert_eq!(
        env.get_as(USER_ID).await,
        json!({"provider_id": null, "model": null, "enabled": false, "supported": true})
    );
}

#[tokio::test]
async fn get_reports_unsupported_when_the_server_has_no_script() {
    let env = Env::without_script().await;

    assert_eq!(
        env.get_as(USER_ID).await,
        json!({"provider_id": null, "model": null, "enabled": false, "supported": false})
    );
}

#[tokio::test]
async fn get_reports_unsupported_when_the_configured_script_file_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let env = Env::with_script_path(dir.path().join("builtin-mcp-image-gen.js")).await;

    assert_eq!(env.get_as(USER_ID).await["supported"], false);
}

/// A directory is not an installed script.
#[tokio::test]
async fn a_directory_is_not_an_installed_script() {
    let dir = tempfile::tempdir().unwrap();
    let env = Env::with_script_path(dir.path().to_path_buf()).await;

    assert_eq!(env.get_as(USER_ID).await["supported"], false);
}

#[tokio::test]
async fn the_administrator_saves_and_every_user_reads_the_same_values() {
    let env = Env::with_script().await;
    env.add_provider(PROVIDER_ID, &[IMAGE_MODEL]).await;

    let saved = env
        .put_ok(json!({"provider_id": PROVIDER_ID, "model": IMAGE_MODEL, "enabled": true}))
        .await;

    let expected = json!({"provider_id": PROVIDER_ID, "model": IMAGE_MODEL, "enabled": true, "supported": true});
    assert_eq!(saved, expected, "the save answers with the stored setting");
    assert_eq!(env.get_as(USER_ID).await, expected, "an ordinary user reads it");
    assert_eq!(env.get_as(ADMIN_ID).await, expected, "so does the administrator");
}

#[tokio::test]
async fn the_setting_is_one_value_for_the_whole_server_and_later_saves_replace_it() {
    let env = Env::with_script().await;
    env.add_provider(PROVIDER_ID, &[IMAGE_MODEL, "dall-e-3"]).await;
    env.enable(PROVIDER_ID, IMAGE_MODEL).await;

    env.put_ok(json!({"provider_id": PROVIDER_ID, "model": "dall-e-3", "enabled": true}))
        .await;

    assert_eq!(env.get_as(USER_ID).await["model"], "dall-e-3");
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM global_settings WHERE key = ?")
        .bind(IMAGE_GENERATION_SETTING_KEY)
        .fetch_one(env.db.pool())
        .await
        .unwrap();
    assert_eq!(rows, 1);
}

#[tokio::test]
async fn the_setting_is_persisted_not_held_in_the_service() {
    let env = Env::with_script().await;
    env.add_provider(PROVIDER_ID, &[IMAGE_MODEL]).await;
    env.enable(PROVIDER_ID, IMAGE_MODEL).await;

    // A brand-new service over the same database, as after a restart.
    let restarted = image_service(&env.db, Some(env.script_path()));
    let read = restarted.get().await.unwrap();

    assert_eq!(read.provider_id.as_deref(), Some(PROVIDER_ID));
    assert_eq!(read.model.as_deref(), Some(IMAGE_MODEL));
    assert!(read.enabled);
}

// ===========================================================================
// PUT /api/settings/image-generation — who may change it
// ===========================================================================

#[tokio::test]
async fn ordinary_users_cannot_change_the_setting() {
    let env = Env::with_script().await;
    env.add_provider(PROVIDER_ID, &[IMAGE_MODEL]).await;

    let resp = env
        .send(request_as(
            Some(USER_ID),
            USER_ID,
            "PUT",
            URI,
            Some(json!({"provider_id": PROVIDER_ID, "model": IMAGE_MODEL, "enabled": true})),
        ))
        .await;

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(resp).await["code"], "FORBIDDEN");
    assert_eq!(env.stored_raw().await, None, "nothing was stored");
    assert_eq!(env.get_as(ADMIN_ID).await["enabled"], false);
}

/// A refused caller learns nothing: the refusal comes before the body is
/// parsed or the provider looked up, so it is the same 403 for a malformed
/// body, an unknown provider and a valid request.
#[tokio::test]
async fn users_are_refused_before_validation_and_lookup() {
    let env = Env::with_script().await;

    let bodies = [
        json!({}),
        json!({"enabled": "yes"}),
        json!({"provider_id": "no-such-provider", "model": "m", "enabled": true}),
    ];
    for body in bodies {
        let resp = env
            .send(request_as(Some(USER_ID), USER_ID, "PUT", URI, Some(body.clone())))
            .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "PUT {body}");
        assert_eq!(body_json(resp).await["code"], "FORBIDDEN", "PUT {body}");
    }
}

#[tokio::test]
async fn the_admin_acting_as_a_user_can_change_the_setting() {
    let env = Env::with_script().await;
    env.add_provider(PROVIDER_ID, &[IMAGE_MODEL]).await;

    // real = admin, current = user-1: the administrator acting as a user.
    let resp = env
        .send(request_as(
            Some(ADMIN_ID),
            USER_ID,
            "PUT",
            URI,
            Some(json!({"provider_id": PROVIDER_ID, "model": IMAGE_MODEL, "enabled": true})),
        ))
        .await;

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(env.get_as(USER_ID).await["enabled"], true);
}

/// The real caller decides, not the effective user: this pairing cannot come
/// out of the auth middleware (only the administrator may act as someone else),
/// so it pins that the route never falls back to `CurrentUser` when a
/// `RealUser` is present.
#[tokio::test]
async fn authorization_follows_the_real_caller_not_the_effective_user() {
    let env = Env::with_script().await;
    env.add_provider(PROVIDER_ID, &[IMAGE_MODEL]).await;

    let resp = env
        .send(request_as(
            Some(USER_ID),
            ADMIN_ID,
            "PUT",
            URI,
            Some(json!({"provider_id": PROVIDER_ID, "model": IMAGE_MODEL, "enabled": true})),
        ))
        .await;

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(resp).await["code"], "FORBIDDEN");
    assert_eq!(env.stored_raw().await, None);
}

/// Routers built without the auth middleware only inject `CurrentUser`; the
/// effective user decides there.
#[tokio::test]
async fn without_a_real_user_the_effective_user_decides() {
    let env = Env::with_script().await;
    env.add_provider(PROVIDER_ID, &[IMAGE_MODEL]).await;
    let body = json!({"provider_id": PROVIDER_ID, "model": IMAGE_MODEL, "enabled": true});

    let resp = env
        .send(request_as(None, USER_ID, "PUT", URI, Some(body.clone())))
        .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let resp = env.send(request_as(None, ADMIN_ID, "PUT", URI, Some(body))).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(env.get_as(USER_ID).await["enabled"], true);
}

// ===========================================================================
// PUT /api/settings/image-generation — what may be saved
// ===========================================================================

#[tokio::test]
async fn enabling_with_an_unknown_provider_is_rejected() {
    let env = Env::with_script().await;

    let message = env
        .put_rejected(json!({"provider_id": "no-such-provider", "model": IMAGE_MODEL, "enabled": true}))
        .await;

    assert!(
        message.contains("no-such-provider"),
        "the message names the provider: {message}"
    );
    assert_eq!(env.stored_raw().await, None, "a rejected save stores nothing");
}

#[tokio::test]
async fn enabling_without_a_provider_is_rejected() {
    let env = Env::with_script().await;

    for body in [
        json!({"provider_id": null, "model": IMAGE_MODEL, "enabled": true}),
        json!({"model": IMAGE_MODEL, "enabled": true}),
        json!({"provider_id": "  ", "model": IMAGE_MODEL, "enabled": true}),
    ] {
        let message = env.put_rejected(body).await;
        assert!(
            message.contains("provider"),
            "the message asks for a provider: {message}"
        );
    }

    assert_eq!(env.stored_raw().await, None);
}

#[tokio::test]
async fn enabling_without_a_model_is_rejected() {
    let env = Env::with_script().await;
    env.add_provider(PROVIDER_ID, &[IMAGE_MODEL]).await;

    for body in [
        json!({"provider_id": PROVIDER_ID, "model": null, "enabled": true}),
        json!({"provider_id": PROVIDER_ID, "model": "", "enabled": true}),
        json!({"provider_id": PROVIDER_ID, "model": "   ", "enabled": true}),
        json!({"provider_id": PROVIDER_ID, "enabled": true}),
    ] {
        let message = env.put_rejected(body).await;
        assert!(message.contains("model"), "the message asks for a model: {message}");
    }

    assert_eq!(env.stored_raw().await, None);
}

#[tokio::test]
async fn enabling_on_a_server_without_the_script_is_rejected() {
    let env = Env::without_script().await;
    env.add_provider(PROVIDER_ID, &[IMAGE_MODEL]).await;

    let message = env
        .put_rejected(json!({"provider_id": PROVIDER_ID, "model": IMAGE_MODEL, "enabled": true}))
        .await;

    assert!(message.contains("not available"), "the message says why: {message}");
    assert_eq!(env.stored_raw().await, None);
    assert_eq!(env.get_as(USER_ID).await["enabled"], false);
}

/// The settings page also offers image models no provider stores (the native
/// Gemini, OpenRouter and Antigravity defaults), so the model is not checked
/// against the provider's model list.
#[tokio::test]
async fn a_model_the_provider_does_not_list_is_accepted() {
    let env = Env::with_script().await;
    env.add_provider(PROVIDER_ID, &["gpt-4o"]).await;

    let saved = env
        .put_ok(json!({"provider_id": PROVIDER_ID, "model": "gemini-2.5-flash-image-preview", "enabled": true}))
        .await;

    assert_eq!(saved["model"], "gemini-2.5-flash-image-preview");
    assert_eq!(saved["enabled"], true);
}

#[tokio::test]
async fn a_provider_without_any_stored_models_is_accepted() {
    let env = Env::with_script().await;
    env.add_provider(PROVIDER_ID, &[]).await;

    let saved = env
        .put_ok(json!({"provider_id": PROVIDER_ID, "model": "google/gemini-2.5-flash-image-preview", "enabled": true}))
        .await;

    assert_eq!(saved["enabled"], true);
}

/// Turning the setting off never needs a working provider, so the administrator
/// can always switch it off or clear it, even after the provider was deleted.
#[tokio::test]
async fn turning_the_setting_off_works_after_its_provider_was_deleted() {
    let env = Env::with_script().await;
    env.add_provider(PROVIDER_ID, &[IMAGE_MODEL]).await;
    env.enable(PROVIDER_ID, IMAGE_MODEL).await;
    env.delete_provider(PROVIDER_ID).await;

    let saved = env
        .put_ok(json!({"provider_id": PROVIDER_ID, "model": "x", "enabled": false}))
        .await;

    // The setting is stored as given, dangling provider and all.
    let expected = json!({"provider_id": PROVIDER_ID, "model": "x", "enabled": false, "supported": true});
    assert_eq!(saved, expected);
    assert_eq!(env.get_as(USER_ID).await, expected);
}

#[tokio::test]
async fn clearing_the_setting_works_after_its_provider_was_deleted() {
    let env = Env::with_script().await;
    env.add_provider(PROVIDER_ID, &[IMAGE_MODEL]).await;
    env.enable(PROVIDER_ID, IMAGE_MODEL).await;
    env.delete_provider(PROVIDER_ID).await;

    env.put_ok(json!({"provider_id": null, "model": null, "enabled": false}))
        .await;

    assert_eq!(
        env.get_as(USER_ID).await,
        json!({"provider_id": null, "model": null, "enabled": false, "supported": true})
    );
}

#[tokio::test]
async fn turning_the_setting_off_never_checks_the_provider() {
    let env = Env::with_script().await;

    // A provider that never existed, and a model nobody lists.
    let saved = env
        .put_ok(json!({"provider_id": "never-existed", "model": "whatever", "enabled": false}))
        .await;

    assert_eq!(saved["provider_id"], "never-existed");
    assert_eq!(saved["model"], "whatever");
    assert_eq!(saved["enabled"], false);
}

#[tokio::test]
async fn turning_the_setting_off_works_on_a_server_without_the_script() {
    let env = Env::without_script().await;

    let saved = env
        .put_ok(json!({"provider_id": null, "model": null, "enabled": false}))
        .await;

    assert_eq!(saved["enabled"], false);
    assert_eq!(saved["supported"], false);
}

#[tokio::test]
async fn blank_provider_and_model_are_stored_as_unset_and_padding_is_trimmed() {
    let env = Env::with_script().await;

    let cleared = env
        .put_ok(json!({"provider_id": "  ", "model": "", "enabled": false}))
        .await;
    assert_eq!(cleared["provider_id"], Value::Null);
    assert_eq!(cleared["model"], Value::Null);

    env.add_provider(PROVIDER_ID, &[IMAGE_MODEL]).await;
    let padded = env
        .put_ok(
            json!({"provider_id": format!("  {PROVIDER_ID} "), "model": format!(" {IMAGE_MODEL}  "), "enabled": true}),
        )
        .await;
    assert_eq!(padded["provider_id"], PROVIDER_ID);
    assert_eq!(padded["model"], IMAGE_MODEL);
}

#[tokio::test]
async fn malformed_bodies_are_bad_requests() {
    let env = Env::with_script().await;

    for body in [
        json!({}),
        json!({"provider_id": null, "model": null}),
        json!({"provider_id": 5, "model": null, "enabled": false}),
        json!({"provider_id": null, "model": null, "enabled": "yes"}),
    ] {
        env.put_rejected(body).await;
    }
    assert_eq!(env.stored_raw().await, None);
}

// ===========================================================================
// ImageGenerationService::session_server
// ===========================================================================

#[tokio::test]
async fn there_is_no_session_server_until_the_setting_is_turned_on() {
    let env = Env::with_script().await;
    env.add_provider(PROVIDER_ID, &[IMAGE_MODEL]).await;
    assert!(env.service.session_server().await.is_none(), "never saved");

    env.put_ok(json!({"provider_id": PROVIDER_ID, "model": IMAGE_MODEL, "enabled": false}))
        .await;
    assert!(env.service.session_server().await.is_none(), "saved but off");
}

#[tokio::test]
async fn the_session_server_launches_the_script_with_the_provider_settings() {
    let env = Env::with_script().await;
    env.add_provider(PROVIDER_ID, &[IMAGE_MODEL]).await;
    env.enable(PROVIDER_ID, IMAGE_MODEL).await;

    let server = env
        .service
        .session_server()
        .await
        .expect("an enabled setting yields a session server");

    assert_eq!(server.id, "aionui-image-generation");
    assert_eq!(server.name, "aionui-image-generation");
    assert_eq!(server.name, IMAGE_GENERATION_MCP_NAME);
    let (command, args, vars) = stdio(&server);
    assert_eq!(command, "/test/bin/node", "the resolved Node program");
    assert_eq!(
        args,
        [
            "--test-flag".to_owned(),
            env.script_path().to_string_lossy().into_owned()
        ],
        "the runtime's argument prefix, then the script"
    );
    assert_eq!(
        vars,
        &HashMap::from([
            ("AIONUI_IMG_PROVIDER_ID".to_owned(), PROVIDER_ID.to_owned()),
            ("AIONUI_IMG_PLATFORM".to_owned(), "openai".to_owned()),
            ("AIONUI_IMG_BASE_URL".to_owned(), PROVIDER_BASE_URL.to_owned()),
            ("AIONUI_IMG_API_KEY".to_owned(), PROVIDER_KEY.to_owned()),
            ("AIONUI_IMG_MODEL".to_owned(), IMAGE_MODEL.to_owned()),
            ("NODE_TEST_ENV".to_owned(), "1".to_owned()),
        ]),
        "the five variables the script reads (the key decrypted) plus the runtime's own"
    );
}

/// The script rotates between several keys, so a stored list goes through untouched.
#[tokio::test]
async fn a_key_list_reaches_the_script_exactly_as_stored() {
    let env = Env::with_script().await;
    env.add_provider_with_key(PROVIDER_ID, &[IMAGE_MODEL], "key-one,key-two\nkey-three")
        .await;
    env.enable(PROVIDER_ID, IMAGE_MODEL).await;

    let server = env.service.session_server().await.unwrap();

    assert_eq!(stdio(&server).2["AIONUI_IMG_API_KEY"], "key-one,key-two\nkey-three");
}

#[tokio::test]
async fn the_session_server_accepts_a_model_the_provider_does_not_list() {
    let env = Env::with_script().await;
    env.add_provider(PROVIDER_ID, &["gpt-4o"]).await;
    env.enable(PROVIDER_ID, "gemini-3-pro-image-1x1").await;

    let server = env
        .service
        .session_server()
        .await
        .expect("a model outside the provider's list is still served");

    assert_eq!(stdio(&server).2["AIONUI_IMG_MODEL"], "gemini-3-pro-image-1x1");
}

#[tokio::test]
async fn the_session_server_follows_later_changes_to_the_setting() {
    let env = Env::with_script().await;
    env.add_provider(PROVIDER_ID, &[IMAGE_MODEL, "dall-e-3"]).await;
    env.add_provider_with_key("prov-other", &["flux"], "sk-other").await;
    env.enable(PROVIDER_ID, IMAGE_MODEL).await;
    let first = env.service.session_server().await.unwrap();
    assert_eq!(stdio(&first).2["AIONUI_IMG_MODEL"], IMAGE_MODEL);

    env.enable("prov-other", "flux").await;
    let second = env.service.session_server().await.unwrap();
    let (_, _, vars) = stdio(&second);
    assert_eq!(vars["AIONUI_IMG_PROVIDER_ID"], "prov-other");
    assert_eq!(vars["AIONUI_IMG_MODEL"], "flux");
    assert_eq!(vars["AIONUI_IMG_API_KEY"], "sk-other");

    env.put_ok(json!({"provider_id": "prov-other", "model": "flux", "enabled": false}))
        .await;
    assert!(env.service.session_server().await.is_none(), "switched off again");
}

#[tokio::test]
async fn there_is_no_session_server_once_the_provider_is_deleted() {
    let env = Env::with_script().await;
    env.add_provider(PROVIDER_ID, &[IMAGE_MODEL]).await;
    env.enable(PROVIDER_ID, IMAGE_MODEL).await;
    assert!(env.service.session_server().await.is_some());

    env.delete_provider(PROVIDER_ID).await;

    assert!(env.service.session_server().await.is_none());
}

#[tokio::test]
async fn there_is_no_session_server_while_the_provider_is_disabled() {
    let env = Env::with_script().await;
    env.add_provider(PROVIDER_ID, &[IMAGE_MODEL]).await;
    env.enable(PROVIDER_ID, IMAGE_MODEL).await;

    env.set_provider_enabled(PROVIDER_ID, false).await;
    assert!(env.service.session_server().await.is_none());

    env.set_provider_enabled(PROVIDER_ID, true).await;
    assert!(
        env.service.session_server().await.is_some(),
        "enabling the provider again brings the tool back without another save"
    );
}

#[tokio::test]
async fn there_is_no_session_server_when_the_script_file_is_gone() {
    let mut env = Env::with_script().await;
    env.add_provider(PROVIDER_ID, &[IMAGE_MODEL]).await;
    env.enable(PROVIDER_ID, IMAGE_MODEL).await;
    assert!(env.service.session_server().await.is_some());

    // Uninstall the script behind the service's back.
    drop(env.script.take());

    assert!(env.service.session_server().await.is_none());
    assert!(!env.service.get().await.unwrap().supported);
    assert!(
        env.service.get().await.unwrap().enabled,
        "the stored choice is kept so the tool returns when the script does"
    );
}

#[tokio::test]
async fn there_is_no_session_server_on_a_server_without_a_script() {
    let env = Env::without_script().await;
    env.add_provider(PROVIDER_ID, &[IMAGE_MODEL]).await;
    // A setting saved while the component existed, e.g. before it was removed.
    env.store_raw(&json!({"provider_id": PROVIDER_ID, "model": IMAGE_MODEL, "enabled": true}).to_string())
        .await;

    assert!(env.service.session_server().await.is_none());
}

#[tokio::test]
async fn there_is_no_session_server_when_the_stored_key_cannot_be_decrypted() {
    let env = Env::with_script().await;
    env.insert_provider(PROVIDER_ID, &[IMAGE_MODEL], "not-an-encrypted-key")
        .await;
    env.enable(PROVIDER_ID, IMAGE_MODEL).await;

    assert!(env.service.session_server().await.is_none());
}

#[tokio::test]
async fn there_is_no_session_server_when_node_cannot_be_found() {
    let env = Env::with_script().await;
    env.add_provider(PROVIDER_ID, &[IMAGE_MODEL]).await;
    env.enable(PROVIDER_ID, IMAGE_MODEL).await;
    let without_node = image_service(&env.db, Some(env.script_path())).with_node_resolver(|| async { None });

    assert!(without_node.session_server().await.is_none());
    assert!(
        env.service.session_server().await.is_some(),
        "the same setting is served when Node is found"
    );
}

/// The setting row can hold anything an older build, a migration or a hand edit
/// left behind; none of it may take a session down.
#[tokio::test]
async fn a_stored_setting_that_cannot_work_yields_no_session_server() {
    let env = Env::with_script().await;
    env.add_provider(PROVIDER_ID, &[IMAGE_MODEL]).await;

    for raw in [
        // Enabled without a model or without a provider.
        json!({"provider_id": PROVIDER_ID, "model": null, "enabled": true}).to_string(),
        json!({"provider_id": PROVIDER_ID, "model": "", "enabled": true}).to_string(),
        json!({"provider_id": null, "model": IMAGE_MODEL, "enabled": true}).to_string(),
        json!({"enabled": true}).to_string(),
    ] {
        env.store_raw(&raw).await;
        assert!(env.service.session_server().await.is_none(), "stored: {raw}");
    }
}

#[tokio::test]
async fn blank_stored_values_read_as_unset() {
    let env = Env::with_script().await;
    env.store_raw(&json!({"provider_id": "  ", "model": "", "enabled": false}).to_string())
        .await;

    assert_eq!(
        env.get_as(USER_ID).await,
        json!({"provider_id": null, "model": null, "enabled": false, "supported": true})
    );
}

#[tokio::test]
async fn a_damaged_stored_setting_reads_as_unset_until_the_administrator_saves_again() {
    let env = Env::with_script().await;
    env.add_provider(PROVIDER_ID, &[IMAGE_MODEL]).await;

    for raw in ["not json", "", "[1, 2]", "\"enabled\"", "null"] {
        env.store_raw(raw).await;
        assert_eq!(
            env.get_as(USER_ID).await,
            json!({"provider_id": null, "model": null, "enabled": false, "supported": true}),
            "stored: {raw:?}"
        );
        assert!(env.service.session_server().await.is_none(), "stored: {raw:?}");
    }

    env.enable(PROVIDER_ID, IMAGE_MODEL).await;
    assert!(
        env.service.session_server().await.is_some(),
        "a new save repairs the row"
    );
}

/// The shape migration 048 writes for an upgraded server is understood as is.
#[tokio::test]
async fn the_value_migration_048_writes_is_understood() {
    let env = Env::with_script().await;
    env.add_provider("p1", &["m1"]).await;
    env.store_raw(r#"{"provider_id":"p1","model":"m1","enabled":true}"#)
        .await;

    assert_eq!(
        env.get_as(USER_ID).await,
        json!({"provider_id": "p1", "model": "m1", "enabled": true, "supported": true})
    );
    let server = env
        .service
        .session_server()
        .await
        .expect("the migrated choice is served");
    assert_eq!(stdio(&server).2["AIONUI_IMG_MODEL"], "m1");
}

/// With the setting off no session pays for locating Node.
#[tokio::test]
async fn node_is_only_looked_up_when_a_server_is_going_to_be_built() {
    let env = Env::with_script().await;
    env.add_provider(PROVIDER_ID, &[IMAGE_MODEL]).await;
    let lookups = Arc::new(AtomicUsize::new(0));
    let counted = {
        let lookups = lookups.clone();
        image_service(&env.db, Some(env.script_path())).with_node_resolver(move || {
            let lookups = lookups.clone();
            async move {
                lookups.fetch_add(1, Ordering::SeqCst);
                Some(fake_node())
            }
        })
    };

    assert!(counted.session_server().await.is_none(), "unset");
    env.put_ok(json!({"provider_id": PROVIDER_ID, "model": IMAGE_MODEL, "enabled": false}))
        .await;
    assert!(counted.session_server().await.is_none(), "off");
    assert_eq!(
        lookups.load(Ordering::SeqCst),
        0,
        "no Node lookup while the setting is off"
    );

    env.enable(PROVIDER_ID, IMAGE_MODEL).await;
    assert!(counted.session_server().await.is_some());
    assert_eq!(lookups.load(Ordering::SeqCst), 1);
}

/// A relative script path is resolved against the backend's working directory
/// once, so the agent process (which runs in a conversation workspace) still
/// finds the script.
#[tokio::test]
async fn a_relative_script_path_reaches_the_agent_as_an_absolute_one() {
    // `cargo test` runs integration tests from the package root, which holds Cargo.toml.
    let env = Env::with_script_path(PathBuf::from("Cargo.toml")).await;
    env.add_provider(PROVIDER_ID, &[IMAGE_MODEL]).await;
    env.enable(PROVIDER_ID, IMAGE_MODEL).await;

    let server = env.service.session_server().await.unwrap();

    let script = stdio(&server).1.last().unwrap();
    assert!(std::path::Path::new(script).is_absolute(), "got {script}");
    assert!(script.ends_with("Cargo.toml"), "got {script}");
}
