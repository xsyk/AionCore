//! The server-wide MCP servers (today: image generation) reach the session
//! options of every kind of agent, are re-read on every assembly and replace
//! whatever a stored snapshot kept under the reserved name.

use super::*;
use crate::SharedSessionMcpSource;
use aionui_api_types::{SessionMcpServer, SessionMcpTransport};

const USER: &str = "user_1";

/// A source that answers with a fixed list, and counts how often it was asked.
struct FixedShared {
    servers: Mutex<Vec<SessionMcpServer>>,
    asked: AtomicUsize,
}

impl FixedShared {
    fn new(servers: Vec<SessionMcpServer>) -> Arc<Self> {
        Arc::new(Self {
            servers: Mutex::new(servers),
            asked: AtomicUsize::new(0),
        })
    }

    fn set(&self, servers: Vec<SessionMcpServer>) {
        *self.servers.lock().unwrap() = servers;
    }

    fn times_asked(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl SharedSessionMcpSource for FixedShared {
    async fn shared_servers(&self) -> Vec<SessionMcpServer> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.servers.lock().unwrap().clone()
    }
}

/// A stdio server; `model` goes into the env so definitions can be told apart.
fn stdio_server(name: &str, model: &str) -> SessionMcpServer {
    SessionMcpServer {
        id: format!("id-{name}"),
        name: name.to_owned(),
        transport: SessionMcpTransport::Stdio {
            command: "/usr/bin/node".to_owned(),
            args: vec!["server.js".to_owned()],
            env: std::collections::HashMap::from([("AIONUI_IMG_MODEL".to_owned(), model.to_owned())]),
        },
    }
}

fn image_server(model: &str) -> SessionMcpServer {
    stdio_server("aionui-image-generation", model)
}

fn model_of(server: &SessionMcpServer) -> &str {
    match &server.transport {
        SessionMcpTransport::Stdio { env, .. } => env["AIONUI_IMG_MODEL"].as_str(),
        other => panic!("stdio server expected, got {other:?}"),
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

fn names(options: &BuildTaskOptions) -> Vec<&str> {
    session_servers(options)
        .iter()
        .map(|server| server.name.as_str())
        .collect()
}

/// Store a conversation of `agent_type` whose snapshot holds `snapshot`.
async fn insert_conversation(
    repo: &Arc<MockRepo>,
    agent_type: &str,
    snapshot: Vec<SessionMcpServer>,
) -> ConversationRow {
    let mut extra = json!({
        "workspace": ensure_test_workspace_path(),
        "session_mcp_servers": snapshot,
    });
    match agent_type {
        "acp" => {
            extra["backend"] = json!("claude");
            extra["agent_id"] = json!("agent-claude");
        }
        "antigravity" => {
            extra["backend"] = json!("antigravity");
            extra["agent_id"] = json!("agent-agy");
        }
        _ => {}
    }
    let row = ConversationRow {
        id: format!("shared-mcp-{}", aionui_common::generate_short_id()),
        user_id: USER.to_owned(),
        name: "shared mcp".to_owned(),
        r#type: agent_type.to_owned(),
        extra: extra.to_string(),
        model: None,
        status: Some("finished".to_owned()),
        source: Some("aionui".to_owned()),
        channel_chat_id: None,
        pinned: false,
        pinned_at: None,
        created_at: 1,
        updated_at: 1,
        project_id: None,
        folder_id: None,
        name_source: None,
    };
    repo.create(&row).await.unwrap();
    row
}

const AGENT_TYPES: [&str; 3] = ["acp", "aionrs", "antigravity"];

#[tokio::test]
async fn every_kind_of_session_gets_the_shared_servers() {
    for agent_type in AGENT_TYPES {
        let (svc, _broadcaster, repo, _task_manager) = make_service();
        svc.with_shared_session_mcp(FixedShared::new(vec![image_server("gpt-image-1")]));
        let row = insert_conversation(&repo, agent_type, Vec::new()).await;

        let options = svc.build_task_options(&row).await.unwrap();

        assert_eq!(names(&options), ["aionui-image-generation"], "{agent_type}");
        assert_eq!(model_of(&session_servers(&options)[0]), "gpt-image-1", "{agent_type}");
    }
}

#[tokio::test]
async fn the_runtime_variant_of_the_build_gets_them_too() {
    for agent_type in AGENT_TYPES {
        let (svc, _broadcaster, repo, _task_manager) = make_service();
        svc.with_shared_session_mcp(FixedShared::new(vec![image_server("gpt-image-1")]));
        let row = insert_conversation(&repo, agent_type, Vec::new()).await;

        let options = svc.build_task_options_for_runtime(&row, None).await.unwrap();

        assert_eq!(names(&options), ["aionui-image-generation"], "{agent_type}");
    }
}

#[tokio::test]
async fn shared_servers_come_after_the_conversations_own_snapshot() {
    for agent_type in AGENT_TYPES {
        let (svc, _broadcaster, repo, _task_manager) = make_service();
        svc.with_shared_session_mcp(FixedShared::new(vec![image_server("gpt-image-1")]));
        let row = insert_conversation(&repo, agent_type, vec![stdio_server("filesystem", "-")]).await;

        let options = svc.build_task_options(&row).await.unwrap();

        assert_eq!(
            names(&options),
            ["filesystem", "aionui-image-generation"],
            "{agent_type}"
        );
    }
}

/// A snapshot saved with the conversation (or an assistant's built-in selection
/// resolved when it was created) must not outlive the shared setting.
#[tokio::test]
async fn a_stale_image_generation_snapshot_gives_way_to_the_shared_definition() {
    for agent_type in AGENT_TYPES {
        let (svc, _broadcaster, repo, _task_manager) = make_service();
        svc.with_shared_session_mcp(FixedShared::new(vec![image_server("shared-model")]));
        let row = insert_conversation(
            &repo,
            agent_type,
            vec![image_server("stale-model"), stdio_server("filesystem", "-")],
        )
        .await;

        let options = svc.build_task_options(&row).await.unwrap();

        assert_eq!(
            names(&options),
            ["filesystem", "aionui-image-generation"],
            "{agent_type}"
        );
        assert_eq!(model_of(&session_servers(&options)[1]), "shared-model", "{agent_type}");
    }
}

#[tokio::test]
async fn a_stale_image_generation_snapshot_is_dropped_while_the_shared_setting_is_off() {
    for agent_type in AGENT_TYPES {
        let (svc, _broadcaster, repo, _task_manager) = make_service();
        svc.with_shared_session_mcp(FixedShared::new(Vec::new()));
        let row = insert_conversation(
            &repo,
            agent_type,
            vec![image_server("stale-model"), stdio_server("filesystem", "-")],
        )
        .await;

        let options = svc.build_task_options(&row).await.unwrap();

        assert_eq!(names(&options), ["filesystem"], "{agent_type}");
    }
}

#[tokio::test]
async fn without_a_shared_source_the_options_are_what_the_conversation_stored() {
    for agent_type in AGENT_TYPES {
        let (svc, _broadcaster, repo, _task_manager) = make_service();
        let row = insert_conversation(&repo, agent_type, vec![stdio_server("filesystem", "-")]).await;

        let options = svc.build_task_options(&row).await.unwrap();

        assert_eq!(names(&options), ["filesystem"], "{agent_type}");
    }
}

/// The source is asked on every assembly, so a setting changed in between shows
/// up in the next session of the same conversation.
#[tokio::test]
async fn every_assembly_asks_the_source_again() {
    let (svc, _broadcaster, repo, _task_manager) = make_service();
    let source = FixedShared::new(vec![image_server("first-model")]);
    svc.with_shared_session_mcp(source.clone());
    let row = insert_conversation(&repo, "aionrs", Vec::new()).await;

    let first = svc.build_task_options(&row).await.unwrap();
    source.set(vec![image_server("second-model")]);
    let second = svc.build_task_options(&row).await.unwrap();
    source.set(Vec::new());
    let third = svc.build_task_options(&row).await.unwrap();

    assert_eq!(model_of(&session_servers(&first)[0]), "first-model");
    assert_eq!(model_of(&session_servers(&second)[0]), "second-model");
    assert!(session_servers(&third).is_empty(), "switched off: no server at all");
    assert_eq!(source.times_asked(), 3);
}

#[tokio::test]
async fn the_conversations_stored_row_is_not_rewritten_by_the_injection() {
    let (svc, _broadcaster, repo, _task_manager) = make_service();
    svc.with_shared_session_mcp(FixedShared::new(vec![image_server("gpt-image-1")]));
    let row = insert_conversation(&repo, "aionrs", vec![stdio_server("filesystem", "-")]).await;

    svc.build_task_options(&row).await.unwrap();

    let stored = repo.get(USER, &row.id).await.unwrap().unwrap();
    let extra: serde_json::Value = serde_json::from_str(&stored.extra).unwrap();
    let stored_names: Vec<&str> = extra["session_mcp_servers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|server| server["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        stored_names,
        ["filesystem"],
        "the shared server lives in the session only"
    );
}
