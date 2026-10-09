//! Integration tests verifying that --work-dir is used for conversation workspace creation.

use std::path::{Path, PathBuf};

use aionui_api_types::CreateConversationRequest;
use aionui_app::{AppConfig, AppServices, build_conversation_state};
use aionui_common::AgentType;
use aionui_db::models::ConversationRow;
use aionui_project::canonical::to_file_uri;

#[tokio::test]
async fn conversation_workspace_uses_work_dir() {
    let data_dir = tempfile::TempDir::new().unwrap();
    let work_dir = tempfile::TempDir::new().unwrap();

    let db = aionui_db::init_database_memory().await.unwrap();
    let config = AppConfig {
        data_dir: data_dir.path().to_path_buf(),
        work_dir: work_dir.path().to_path_buf(),
        local: true,
        ..Default::default()
    };
    let services = AppServices::from_config(db, &config).await.unwrap();
    let state = build_conversation_state(&services, None, None);

    let request = CreateConversationRequest {
        r#type: Some(AgentType::Acp),
        name: Some("test".to_string()),
        model: None,
        assistant: None,
        source: None,
        channel_chat_id: None,
        extra: serde_json::json!({}),
    };
    let response = state.service.create("system_default_user", request).await.unwrap();

    let workspace = response.extra.get("workspace").and_then(|v| v.as_str()).unwrap();
    assert!(
        workspace.starts_with(work_dir.path().to_str().unwrap()),
        "workspace should be under work_dir, got: {workspace}"
    );
    assert!(
        !workspace.starts_with(data_dir.path().to_str().unwrap()),
        "workspace should NOT be under data_dir, got: {workspace}"
    );
}

#[tokio::test]
async fn user_specified_workspace_is_not_overridden() {
    let data_dir = tempfile::TempDir::new().unwrap();
    let work_dir = tempfile::TempDir::new().unwrap();
    let custom_workspace = tempfile::TempDir::new().unwrap();

    let db = aionui_db::init_database_memory().await.unwrap();
    let config = AppConfig {
        data_dir: data_dir.path().to_path_buf(),
        work_dir: work_dir.path().to_path_buf(),
        local: true,
        ..Default::default()
    };
    let services = AppServices::from_config(db, &config).await.unwrap();
    let state = build_conversation_state(&services, None, None);

    let request = CreateConversationRequest {
        r#type: Some(AgentType::Acp),
        name: Some("test".to_string()),
        model: None,
        assistant: None,
        source: None,
        channel_chat_id: None,
        extra: serde_json::json!({
            "workspace": custom_workspace.path().to_str().unwrap()
        }),
    };
    let response = state.service.create("system_default_user", request).await.unwrap();

    let workspace = response.extra.get("workspace").and_then(|v| v.as_str()).unwrap();
    assert!(
        workspace.starts_with(custom_workspace.path().to_str().unwrap()),
        "workspace should use user-specified path, got: {workspace}"
    );
}

#[tokio::test]
async fn workspace_defaults_to_data_dir_when_work_dir_equals_data_dir() {
    let data_dir = tempfile::TempDir::new().unwrap();

    let db = aionui_db::init_database_memory().await.unwrap();
    let config = AppConfig {
        data_dir: data_dir.path().to_path_buf(),
        work_dir: data_dir.path().to_path_buf(),
        local: true,
        ..Default::default()
    };
    let services = AppServices::from_config(db, &config).await.unwrap();
    let state = build_conversation_state(&services, None, None);

    let request = CreateConversationRequest {
        r#type: Some(AgentType::Acp),
        name: Some("test".to_string()),
        model: None,
        assistant: None,
        source: None,
        channel_chat_id: None,
        extra: serde_json::json!({}),
    };
    let response = state.service.create("system_default_user", request).await.unwrap();

    let workspace = response.extra.get("workspace").and_then(|v| v.as_str()).unwrap();
    assert!(
        workspace.starts_with(data_dir.path().to_str().unwrap()),
        "workspace should be under data_dir when work_dir == data_dir, got: {workspace}"
    );
}

// ── The work dir moved away from the data dir (1.0.0 -> 1.0.1 on servers) ──
//
// Conversations created before the move carry absolute workspace paths under
// `<data_dir>/conversations/...`. The server must keep treating those as its own
// temp workspaces: clean them up with the conversation, and not mistake them for
// user projects.

/// Store a conversation the way 1.0.0 left it: auto workspace under the data dir.
async fn seed_conversation_with_workspace(services: &AppServices, id: &str, workspace: &Path) {
    services
        .conversation_repo
        .create(&ConversationRow {
            id: id.to_owned(),
            user_id: "system_default_user".to_owned(),
            name: "created before the work dir moved".to_owned(),
            r#type: "acp".to_owned(),
            extra: serde_json::json!({ "workspace": workspace, "backend": "claude" }).to_string(),
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
        })
        .await
        .unwrap();
}

fn previous_root_workspace(data_dir: &Path, id: &str) -> PathBuf {
    data_dir
        .join("conversations/users/system_default_user/2026/10/01")
        .join(format!("claude-temp-{id}"))
}

async fn services_with_moved_work_dir(data_dir: &Path, work_dir: &Path) -> AppServices {
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = AppConfig {
        data_dir: data_dir.to_path_buf(),
        work_dir: work_dir.to_path_buf(),
        local: true,
        ..Default::default()
    };
    AppServices::from_config(db, &config).await.unwrap()
}

#[tokio::test]
async fn deleting_a_conversation_removes_its_workspace_under_the_previous_data_dir() {
    let data_dir = tempfile::TempDir::new().unwrap();
    let work_dir = tempfile::TempDir::new().unwrap();
    let services = services_with_moved_work_dir(data_dir.path(), work_dir.path()).await;

    let workspace = previous_root_workspace(data_dir.path(), "conv-old");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("scratch.txt"), "left by a 1.0.0 session").unwrap();
    seed_conversation_with_workspace(&services, "conv-old", &workspace).await;

    services
        .conversation_service
        .delete("system_default_user", "conv-old")
        .await
        .unwrap();

    assert!(
        !workspace.exists(),
        "the old auto workspace must go with its conversation"
    );
    assert!(
        !data_dir
            .path()
            .join("conversations/users/system_default_user/2026")
            .exists(),
        "empty date directories under the previous root are pruned too"
    );
    services.database.close().await;
}

#[tokio::test]
async fn deleting_a_conversation_keeps_a_user_folder_that_sits_under_the_previous_data_dir() {
    let data_dir = tempfile::TempDir::new().unwrap();
    let work_dir = tempfile::TempDir::new().unwrap();
    let services = services_with_moved_work_dir(data_dir.path(), work_dir.path()).await;

    // Not in the auto layout, so it is somebody's own folder even though it
    // lives below the previous root's `conversations/`.
    let user_folder = data_dir.path().join("conversations/my-project/claude-temp-conv-mine");
    std::fs::create_dir_all(&user_folder).unwrap();
    seed_conversation_with_workspace(&services, "conv-mine", &user_folder).await;

    services
        .conversation_service
        .delete("system_default_user", "conv-mine")
        .await
        .unwrap();

    assert!(user_folder.is_dir());
    services.database.close().await;
}

#[tokio::test]
async fn project_binding_classifies_the_previous_data_dir_workspaces_as_temp() {
    let data_dir = tempfile::TempDir::new().unwrap();
    let work_dir = tempfile::TempDir::new().unwrap();
    let services = services_with_moved_work_dir(data_dir.path(), work_dir.path()).await;

    let old_workspace = previous_root_workspace(data_dir.path(), "conv-old");
    std::fs::create_dir_all(&old_workspace).unwrap();
    let temp = services
        .project_service
        .resolve_existing("system_default_user", to_file_uri(&old_workspace).unwrap())
        .await
        .unwrap();
    assert_eq!(temp.project.kind, "temp");

    // Workspaces under the current work dir are temp, as before.
    let current_workspace = work_dir
        .path()
        .join("conversations/users/u/2026/10/02/claude-temp-conv-new");
    std::fs::create_dir_all(&current_workspace).unwrap();
    let current = services
        .project_service
        .resolve_existing("system_default_user", to_file_uri(&current_workspace).unwrap())
        .await
        .unwrap();
    assert_eq!(current.project.kind, "temp");

    // A folder the user picked is a standard project.
    let picked = tempfile::TempDir::new().unwrap();
    let standard = services
        .project_service
        .resolve_existing("system_default_user", to_file_uri(picked.path()).unwrap())
        .await
        .unwrap();
    assert_eq!(standard.project.kind, "standard");
    services.database.close().await;
}

#[tokio::test]
async fn new_conversations_are_provisioned_under_the_work_dir_not_the_previous_data_dir() {
    let data_dir = tempfile::TempDir::new().unwrap();
    let work_dir = tempfile::TempDir::new().unwrap();
    let services = services_with_moved_work_dir(data_dir.path(), work_dir.path()).await;
    let state = build_conversation_state(&services, None, None);

    let request = CreateConversationRequest {
        r#type: Some(AgentType::Acp),
        name: Some("test".to_string()),
        model: None,
        assistant: None,
        source: None,
        channel_chat_id: None,
        extra: serde_json::json!({}),
    };
    let response = state.service.create("system_default_user", request).await.unwrap();

    let workspace = response.extra.get("workspace").and_then(|v| v.as_str()).unwrap();
    assert!(
        Path::new(workspace).starts_with(work_dir.path().join("conversations")),
        "workspace should be under the work dir, got: {workspace}"
    );
    assert!(
        !data_dir.path().join("conversations").exists(),
        "nothing may be provisioned under the previous data dir"
    );
    services.database.close().await;
}
