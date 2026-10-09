//! The shared image generation setting reaches the sessions.
//!
//! Through the real app wiring: the administrator saves the setting over HTTP,
//! and the session options the conversation service assembles for any user's
//! conversation, of any agent kind, carry the image generation MCP server built
//! from it. Switching the setting off takes the server away again, also from a
//! conversation whose stored snapshot still holds an old copy.
//!
//! The server needs the script file named by `AIONUI_IMAGE_GEN_MCP_SCRIPT` at
//! startup (a temp file here) and a Node program, which the test fixes to a
//! fake path so no runtime has to be installed. The variable is process-wide,
//! so every test holds `ScriptEnv` for its whole body.

mod common;

use std::path::PathBuf;
use std::sync::OnceLock;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tokio::sync::{Mutex, MutexGuard};
use tower::ServiceExt;

use aionui_ai_agent::AgentSessionKind;
use aionui_ai_agent::types::BuildTaskOptions;
use aionui_api_types::{SessionMcpServer, SessionMcpTransport};
use aionui_app::{AppConfig, AppServices, create_router};
use aionui_db::models::ConversationRow;
use aionui_runtime::ResolvedCommand;
use common::{body_json, json_with_token, setup_and_login};

const PASSWORD: &str = "StrongP@ss1";
const SCRIPT_ENV: &str = "AIONUI_IMAGE_GEN_MCP_SCRIPT";
const URI: &str = "/api/settings/image-generation";
const PROVIDER_KEY: &str = "sk-e2e-image";

static ENV_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();

/// Names the image generation script in the environment until dropped, and keeps
/// the file alive. Holds the process-wide lock for the whole time.
struct ScriptEnv {
    _guard: MutexGuard<'static, ()>,
    script: tempfile::NamedTempFile,
}

impl ScriptEnv {
    async fn install() -> Self {
        let guard = ENV_MUTEX.get_or_init(|| Mutex::new(())).lock().await;
        let script = tempfile::Builder::new().suffix(".js").tempfile().unwrap();
        // SAFETY: the variable is only touched while holding ENV_MUTEX.
        unsafe {
            std::env::set_var(SCRIPT_ENV, script.path());
        }
        Self { _guard: guard, script }
    }
}

impl Drop for ScriptEnv {
    fn drop(&mut self) {
        // SAFETY: the lock in `_guard` is still held; it is released after this runs.
        unsafe {
            std::env::remove_var(SCRIPT_ENV);
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
    env: ScriptEnv,
}

impl Harness {
    async fn new() -> Self {
        let env = ScriptEnv::install().await;
        let db = aionui_db::init_database_memory().await.unwrap();
        let services = AppServices::from_config(db, &AppConfig::default()).await.unwrap();
        // The machine running the test may have no Node runtime to find.
        let image_generation = services
            .image_generation_service
            .clone()
            .with_node_resolver(|| async { Some(ResolvedCommand::plain(PathBuf::from("/test/bin/node"))) });
        let services = services.with_image_generation_service(image_generation);
        let mut app = create_router(&services).await.expect("build router");
        let (token, csrf) = setup_and_login(&mut app, &services, "admin", PASSWORD).await;
        let admin = Session { token, csrf };
        let (token, csrf) = setup_and_login(&mut app, &services, "alice", PASSWORD).await;
        let alice = Session { token, csrf };
        Self {
            app,
            services,
            admin,
            alice,
            env,
        }
    }

    async fn send(&self, req: Request<Body>) -> (StatusCode, Value) {
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    /// The administrator adds an image provider and returns its id.
    async fn admin_adds_provider(&self) -> String {
        let req = json_with_token(
            "POST",
            "/api/providers",
            json!({
                "platform": "openai",
                "name": "Images",
                "base_url": "https://img.example.com/v1",
                "api_key": PROVIDER_KEY,
                "models": ["gpt-image-1"]
            }),
            &self.admin.token,
            &self.admin.csrf,
        );
        let (status, json) = self.send(req).await;
        assert_eq!(status, StatusCode::CREATED, "{json}");
        json["data"]["id"].as_str().unwrap().to_owned()
    }

    /// The administrator saves the image generation setting.
    async fn admin_saves(&self, body: Value) {
        let req = json_with_token("PUT", URI, body, &self.admin.token, &self.admin.csrf);
        let (status, json) = self.send(req).await;
        assert_eq!(status, StatusCode::OK, "{json}");
    }

    /// A new conversation of `agent_type` for `username`, as stored.
    async fn conversation(&self, session: &Session, username: &str, agent_type: &str) -> ConversationRow {
        let req = json_with_token(
            "POST",
            "/api/conversations",
            json!({"type": agent_type, "extra": {}}),
            &session.token,
            &session.csrf,
        );
        let (status, json) = self.send(req).await;
        assert_eq!(status, StatusCode::CREATED, "{agent_type}: {json}");
        let id = json["data"]["id"].as_str().unwrap();
        let user = self
            .services
            .user_repo
            .find_by_username(username)
            .await
            .unwrap()
            .expect("user exists");
        self.services
            .conversation_repo
            .get(&user.id, id)
            .await
            .unwrap()
            .expect("the conversation was stored")
    }

    /// The session options the conversation service assembles for `row` now.
    async fn options(&self, row: &ConversationRow) -> BuildTaskOptions {
        self.services
            .conversation_service
            .build_task_options_for_runtime(row, None)
            .await
            .unwrap()
    }
}

/// The session MCP servers of any kind of agent.
fn session_servers(options: &BuildTaskOptions) -> &[SessionMcpServer] {
    match &options.context.kind {
        AgentSessionKind::Acp(ctx) => &ctx.config.session_mcp_servers,
        AgentSessionKind::Antigravity(ctx) => &ctx.config.session_mcp_servers,
        AgentSessionKind::Aionrs(ctx) => &ctx.config.session_mcp_servers,
    }
}

fn image_generation(options: &BuildTaskOptions) -> Option<&SessionMcpServer> {
    session_servers(options)
        .iter()
        .find(|server| server.name == "aionui-image-generation")
}

#[tokio::test]
async fn sessions_get_the_image_generation_server_once_the_administrator_turns_it_on() {
    let h = Harness::new().await;
    let provider_id = h.admin_adds_provider().await;
    let alice_aionrs = h.conversation(&h.alice, "alice", "aionrs").await;
    let alice_acp = h.conversation(&h.alice, "alice", "acp").await;
    let admin_aionrs = h.conversation(&h.admin, "admin", "aionrs").await;

    for row in [&alice_aionrs, &alice_acp, &admin_aionrs] {
        assert!(
            image_generation(&h.options(row).await).is_none(),
            "{}: nothing is configured yet",
            row.r#type
        );
    }

    h.admin_saves(json!({"provider_id": provider_id, "model": "gpt-image-1", "enabled": true}))
        .await;

    // The setting is shared: the conversations of both users, of both agent
    // kinds, and ones created before the setting existed all get the server.
    for row in [&alice_aionrs, &alice_acp, &admin_aionrs] {
        let options = h.options(row).await;
        let server = image_generation(&options).unwrap_or_else(|| panic!("{} gets the server", row.r#type));
        assert_eq!(server.id, "aionui-image-generation");
        let SessionMcpTransport::Stdio { command, args, env } = &server.transport else {
            panic!("stdio server expected, got {:?}", server.transport);
        };
        assert_eq!(command, "/test/bin/node");
        assert_eq!(args, &[h.env.script.path().to_string_lossy().into_owned()]);
        assert_eq!(env["AIONUI_IMG_PROVIDER_ID"], provider_id);
        assert_eq!(env["AIONUI_IMG_PLATFORM"], "openai");
        assert_eq!(env["AIONUI_IMG_BASE_URL"], "https://img.example.com/v1");
        assert_eq!(
            env["AIONUI_IMG_API_KEY"], PROVIDER_KEY,
            "the key is decrypted for the script"
        );
        assert_eq!(env["AIONUI_IMG_MODEL"], "gpt-image-1");
    }
}

#[tokio::test]
async fn switching_the_setting_off_takes_the_server_away_again() {
    let h = Harness::new().await;
    let provider_id = h.admin_adds_provider().await;
    let row = h.conversation(&h.alice, "alice", "aionrs").await;
    h.admin_saves(json!({"provider_id": provider_id, "model": "gpt-image-1", "enabled": true}))
        .await;
    assert!(image_generation(&h.options(&row).await).is_some());

    h.admin_saves(json!({"provider_id": provider_id, "model": "gpt-image-1", "enabled": false}))
        .await;

    assert!(image_generation(&h.options(&row).await).is_none());
}

#[tokio::test]
async fn a_conversation_does_not_keep_an_old_copy_of_the_server_when_the_setting_is_off() {
    let h = Harness::new().await;
    let mut row = h.conversation(&h.alice, "alice", "aionrs").await;
    // What a conversation created while an earlier build (or the desktop's
    // per-user built-in row) was in charge may still hold.
    let mut extra: Value = serde_json::from_str(&row.extra).unwrap();
    extra["session_mcp_servers"] = json!([{
        "id": "builtin-image-gen",
        "name": "aionui-image-generation",
        "transport": {
            "type": "stdio",
            "command": "node",
            "args": ["old-image-gen.js"],
            "env": {"AIONUI_IMG_API_KEY": "sk-from-the-old-snapshot"}
        }
    }]);
    row.extra = extra.to_string();

    assert!(
        image_generation(&h.options(&row).await).is_none(),
        "the setting is off, so the snapshot's copy must not start"
    );
}

#[tokio::test]
async fn a_server_without_the_image_generation_script_gives_sessions_nothing() {
    let h = Harness::new().await;
    let provider_id = h.admin_adds_provider().await;
    let row = h.conversation(&h.alice, "alice", "aionrs").await;
    h.admin_saves(json!({"provider_id": provider_id, "model": "gpt-image-1", "enabled": true}))
        .await;
    assert!(image_generation(&h.options(&row).await).is_some());

    // The script goes away (the install is damaged): sessions start without
    // the tool instead of failing.
    std::fs::remove_file(h.env.script.path()).unwrap();

    assert!(image_generation(&h.options(&row).await).is_none());
}
