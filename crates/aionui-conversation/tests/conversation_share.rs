use std::sync::Arc;

use aionui_api_types::{PublicShareMessagesQuery, ShareResourceQuery};
use aionui_conversation::{ConversationShareService, ShareRouterState, public_conversation_share_routes};
use aionui_db::{
    IConversationRepository, SqliteConversationRepository, SqliteConversationShareRepository, init_database_memory,
};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use tower::ServiceExt;

const OWNER: &str = "system_default_user";

async fn fixture() -> (ConversationShareService, aionui_db::Database, tempfile::TempDir) {
    let db = init_database_memory().await.unwrap();
    let workspace = tempfile::tempdir().unwrap();
    sqlx::query("INSERT INTO conversations (id,user_id,name,type,extra,created_at,updated_at) VALUES ('shared',?,'Shared','acp',?,1,1)")
        .bind(OWNER)
        .bind(json!({"workspace":workspace.path(),"preset_rules":"PRIVATE","api_key":"PRIVATE"}).to_string())
        .execute(db.pool()).await.unwrap();
    let service = ConversationShareService::new(Arc::new(SqliteConversationShareRepository::new(db.pool().clone())));
    (service, db, workspace)
}

async fn insert(db: &aionui_db::Database, id: &str, content: Value, hidden: bool) {
    sqlx::query("INSERT INTO messages (id,conversation_id,type,content,position,status,hidden,created_at) VALUES (?,'shared','text',?,'left','finish',?,1)")
        .bind(id).bind(content.to_string()).bind(hidden).execute(db.pool()).await.unwrap();
}

#[tokio::test]
async fn share_public_data_excludes_hidden_messages_and_configuration() {
    let (service, db, _workspace) = fixture().await;
    insert(&db, "visible", json!({"content":"Visible"}), false).await;
    insert(&db, "hidden", json!({"content":"PRIVATE"}), true).await;
    let share = service.set_enabled(OWNER, "shared", true).await.unwrap();
    let token = share.token.unwrap();
    assert_eq!(token.len(), 43);
    let metadata = serde_json::to_value(service.public_conversation(&token).await.unwrap()).unwrap();
    assert_eq!(metadata["name"], "Shared");
    assert!(metadata.get("extra").is_none());
    assert!(metadata.get("model").is_none());
    assert!(metadata.get("user_id").is_none());
    let messages = service
        .public_messages(&token, PublicShareMessagesQuery::default())
        .await
        .unwrap();
    assert_eq!(messages.items.len(), 1);
    assert_eq!(messages.items[0].content["content"], "Visible");
    service.set_enabled(OWNER, "shared", false).await.unwrap();
    assert!(service.public_conversation(&token).await.is_err());
    let restored = service.set_enabled(OWNER, "shared", true).await.unwrap();
    assert_eq!(restored.token.as_deref(), Some(token.as_str()));
}

#[tokio::test]
async fn resource_requires_a_visible_reference_and_an_owned_root() {
    let (service, db, workspace) = fixture().await;
    let attached = workspace.path().join("picture.png");
    let unmentioned = workspace.path().join("private.txt");
    tokio::fs::write(&attached, b"image-bytes").await.unwrap();
    tokio::fs::write(&unmentioned, b"PRIVATE").await.unwrap();
    insert(
        &db,
        "visible",
        json!({"content":format!("![image]({})", attached.display())}),
        false,
    )
    .await;
    let token = service.set_enabled(OWNER, "shared", true).await.unwrap().token.unwrap();
    let allowed = service
        .resource(
            &token,
            &ShareResourceQuery {
                path: attached.to_string_lossy().into_owned(),
                download: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(allowed.path, attached.canonicalize().unwrap());
    assert!(
        service
            .resource(
                &token,
                &ShareResourceQuery {
                    path: unmentioned.to_string_lossy().into_owned(),
                    download: false
                }
            )
            .await
            .is_err()
    );
    assert!(
        service
            .resource(
                &token,
                &ShareResourceQuery {
                    path: "/etc/passwd".into(),
                    download: false
                }
            )
            .await
            .is_err()
    );
    assert!(
        service
            .resource(
                &token,
                &ShareResourceQuery {
                    path: "../private.txt".into(),
                    download: false
                }
            )
            .await
            .is_err()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn resource_rejects_symlink_escape_even_when_named_in_the_message() {
    let (service, db, workspace) = fixture().await;
    let other = tempfile::tempdir().unwrap();
    let secret = other.path().join("secret.txt");
    tokio::fs::write(&secret, b"PRIVATE").await.unwrap();
    let link = workspace.path().join("link.txt");
    std::os::unix::fs::symlink(&secret, &link).unwrap();
    insert(
        &db,
        "visible",
        json!({"content":format!("[file]({})", link.display())}),
        false,
    )
    .await;
    let token = service.set_enabled(OWNER, "shared", true).await.unwrap().token.unwrap();
    assert!(
        service
            .resource(
                &token,
                &ShareResourceQuery {
                    path: link.to_string_lossy().into_owned(),
                    download: true
                }
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn anonymous_routes_have_no_mutations_and_disabled_links_return_404() {
    let (service, db, _workspace) = fixture().await;
    insert(&db, "visible", json!({"content":"Hello"}), false).await;
    let token = service.set_enabled(OWNER, "shared", true).await.unwrap().token.unwrap();
    let app = public_conversation_share_routes(ShareRouterState {
        service: service.clone(),
    });
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/public/shares/current/messages")
                .header("x-share-token", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "private, no-store");
    let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), 1_000_000).await.unwrap()).unwrap();
    assert_eq!(body["data"]["items"][0]["content"]["content"], "Hello");
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/public/shares/current/messages")
                .header("x-share-token", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    service.set_enabled(OWNER, "shared", false).await.unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/public/shares/current")
                .header("x-share-token", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn public_read_does_not_backfill_or_modify_the_original_conversation() {
    let (service, db, _workspace) = fixture().await;
    let conversations = SqliteConversationRepository::new(db.pool().clone());
    let before = conversations.get(OWNER, "shared").await.unwrap().unwrap();
    let token = service.set_enabled(OWNER, "shared", true).await.unwrap().token.unwrap();
    service.public_conversation(&token).await.unwrap();
    service
        .public_messages(&token, PublicShareMessagesQuery::default())
        .await
        .unwrap();
    let after = conversations.get(OWNER, "shared").await.unwrap().unwrap();
    assert_eq!(before.extra, after.extra);
    assert_eq!(before.updated_at, after.updated_at);
    assert_eq!(before.project_id, after.project_id);
}

#[tokio::test]
async fn share_credential_is_accepted_only_in_the_header() {
    let (service, _db, _workspace) = fixture().await;
    let token = service.set_enabled(OWNER, "shared", true).await.unwrap().token.unwrap();
    let app = public_conversation_share_routes(ShareRouterState { service });
    for uri in [
        "/api/public/shares/current".to_owned(),
        format!("/api/public/shares/current?token={token}"),
        format!("/api/public/shares/{token}"),
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/public/shares/current")
                .header("x-share-token", "unknown")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn hidden_references_do_not_authorize_downloads_and_html_is_forced_to_download() {
    let (service, db, workspace) = fixture().await;
    let hidden = workspace.path().join("hidden.txt");
    let html = workspace.path().join("page.html");
    tokio::fs::write(&hidden, b"PRIVATE").await.unwrap();
    tokio::fs::write(&html, b"<script>alert(1)</script>").await.unwrap();
    insert(
        &db,
        "hidden",
        json!({"content":format!("[file]({})", hidden.display())}),
        true,
    )
    .await;
    insert(
        &db,
        "visible",
        json!({"content":format!("[file]({})", html.display())}),
        false,
    )
    .await;
    let token = service.set_enabled(OWNER, "shared", true).await.unwrap().token.unwrap();
    assert!(
        service
            .resource(
                &token,
                &ShareResourceQuery {
                    path: hidden.to_string_lossy().into_owned(),
                    download: false
                }
            )
            .await
            .is_err()
    );
    let app = public_conversation_share_routes(ShareRouterState { service });
    // Only the fixture path is percent encoded; the capability never enters a URI.
    let path = html
        .to_string_lossy()
        .bytes()
        .map(|byte| format!("%{byte:02X}"))
        .collect::<String>();
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/public/shares/current/resources?path={path}"))
                .header("x-share-token", token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "application/octet-stream");
    assert!(
        response.headers()["content-disposition"]
            .to_str()
            .unwrap()
            .starts_with("attachment;")
    );
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(
        response.headers()["content-security-policy"],
        "default-src 'none'; sandbox"
    );
}

#[tokio::test]
async fn public_artifacts_follow_share_authorization_and_have_no_write_route() {
    let (service, db, _workspace) = fixture().await;
    sqlx::query("INSERT INTO conversation_artifacts (id,conversation_id,kind,status,payload,created_at,updated_at) VALUES ('skill','shared','skill_suggest','pending','{\"name\":\"Visible skill\"}',2,2)")
        .execute(db.pool()).await.unwrap();
    insert(
        &db,
        "legacy",
        json!({"cronJobId":"cron","name":"Visible trigger"}),
        false,
    )
    .await;
    insert(&db, "hidden-trigger", json!({"name":"PRIVATE"}), true).await;
    sqlx::query("UPDATE messages SET type = 'cron_trigger'")
        .execute(db.pool())
        .await
        .unwrap();
    let token = service.set_enabled(OWNER, "shared", true).await.unwrap().token.unwrap();
    let app = public_conversation_share_routes(ShareRouterState {
        service: service.clone(),
    });
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/public/shares/current/artifacts")
                .header("x-share-token", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), 1_000_000).await.unwrap()).unwrap();
    assert_eq!(body["data"].as_array().unwrap().len(), 2);
    assert_eq!(body["data"][0]["id"], "legacy-cron-trigger:legacy");
    assert_eq!(body["data"][1]["payload"]["name"], "Visible skill");
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/public/shares/current/artifacts")
                .header("x-share-token", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    service.set_enabled(OWNER, "shared", false).await.unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/public/shares/current/artifacts")
                .header("x-share-token", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn tool_inputs_and_permission_parameters_do_not_authorize_resources() {
    let (service, db, workspace) = fixture().await;
    let token = service.set_enabled(OWNER, "shared", true).await.unwrap().token.unwrap();
    let app = public_conversation_share_routes(ShareRouterState { service });
    for (index, (kind, status)) in [
        ("acp_permission", "pending"),
        ("permission", "pending"),
        ("tool_call", "running"),
        ("acp_tool_call", "pending"),
        ("tool_call", "completed"),
        ("acp_tool_call", "completed"),
    ]
    .into_iter()
    .enumerate()
    {
        let secret = workspace.path().join(format!("private-{index}.env"));
        tokio::fs::write(&secret, b"PRIVATE_API_KEY").await.unwrap();
        let path = secret.to_string_lossy().into_owned();
        // Neither a plain path nor attachment-shaped data inside untrusted tool
        // inputs may become a grant to download the file's full contents.
        let input = json!({"path":path,"file_path":path,"files":[path],"description":format!("[file]({path})\n[[AION_FILES]]\n{path}")});
        let content = match kind {
            "acp_permission" => json!({"tool_call":{"tool_call_id":"pending-read","raw_input":input},"options":[]}),
            "permission" => json!({"title":"Read file?","raw_input":input}),
            "tool_call" => json!({"call_id":"read","name":"Read","status":status,"input":input,"args":input}),
            _ => {
                json!({"update":{"tool_call_id":"read","status":status,"rawInput":input,"raw_input":input,"locations":[{"path":path}],"content":[{"type":"diff","path":path,"new_text":"pending"}]}})
            }
        };
        let id = format!("input-{index}");
        insert(&db, &id, content, false).await;
        sqlx::query("UPDATE messages SET type=? WHERE id=?")
            .bind(kind)
            .bind(&id)
            .execute(db.pool())
            .await
            .unwrap();
        let encoded = path.bytes().map(|byte| format!("%{byte:02X}")).collect::<String>();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/public/shares/current/resources?path={encoded}"))
                    .header("x-share-token", &token)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{kind} {status} input granted a resource"
        );
    }
}

#[tokio::test]
async fn explicit_attachments_and_completed_tool_image_outputs_authorize_resources() {
    let (service, db, workspace) = fixture().await;
    let attachment = workspace.path().join("attachment.txt");
    let generated = workspace.path().join("generated.png");
    tokio::fs::write(&attachment, b"attachment").await.unwrap();
    tokio::fs::write(&generated, b"image").await.unwrap();
    insert(
        &db,
        "attachment",
        json!({"content":"Attached", "files":[attachment]}),
        false,
    )
    .await;
    // This is the persisted image output shape exercised by the stream relay's
    // run_acp_image_tool_call_update_persists_finish_without_base64 fixture.
    insert(&db, "image", json!({"update":{"tool_call_id":"image","status":"completed","rawOutput":{"image":{"path":generated,"mime_type":"image/png","source":"codex_image_generation"}}}}), false).await;
    sqlx::query("UPDATE messages SET type='acp_tool_call' WHERE id='image'")
        .execute(db.pool())
        .await
        .unwrap();
    let token = service.set_enabled(OWNER, "shared", true).await.unwrap().token.unwrap();
    let app = public_conversation_share_routes(ShareRouterState { service });
    for path in [attachment, generated] {
        let encoded = path
            .to_string_lossy()
            .bytes()
            .map(|byte| format!("%{byte:02X}"))
            .collect::<String>();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/public/shares/current/resources?path={encoded}"))
                    .header("x-share-token", &token)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn markdown_resource_destinations_support_spaces_parentheses_and_single_uri_decoding() {
    let (service, db, workspace) = fixture().await;
    let cases = [
        ("my image.png", "![image](<my image.png>)"),
        ("plot(1).png", "![image](plot(1).png)"),
        ("encoded space.png", "![image](encoded%20space.png)"),
        ("literal%20name.png", "![image](literal%2520name.png)"),
    ];
    for (index, (filename, markdown)) in cases.iter().enumerate() {
        tokio::fs::write(workspace.path().join(filename), b"image")
            .await
            .unwrap();
        insert(&db, &format!("markdown-{index}"), json!({"content":markdown}), false).await;
    }
    let token = service.set_enabled(OWNER, "shared", true).await.unwrap().token.unwrap();
    let app = public_conversation_share_routes(ShareRouterState { service });
    for (filename, _) in cases {
        let encoded = workspace
            .path()
            .join(filename)
            .to_string_lossy()
            .bytes()
            .map(|byte| format!("%{byte:02X}"))
            .collect::<String>();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/public/shares/current/resources?path={encoded}"))
                    .header("x-share-token", &token)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "unsupported Markdown resource: {filename}"
        );
    }
}

#[tokio::test]
async fn paths_in_markdown_code_and_generic_tool_output_are_not_download_grants() {
    let (service, db, workspace) = fixture().await;
    let token = service.set_enabled(OWNER, "shared", true).await.unwrap().token.unwrap();
    let app = public_conversation_share_routes(ShareRouterState { service });
    for (index, (kind, content)) in [("text", "code"), ("acp_tool_call", "output")].into_iter().enumerate() {
        let file = workspace.path().join(format!("private-code-{index}.env"));
        tokio::fs::write(&file, b"PRIVATE").await.unwrap();
        let path = file.to_string_lossy().into_owned();
        let value = if content == "code" {
            json!({"content":format!("`[file]({path})`\n\n```markdown\n![image]({path})\n```")})
        } else {
            json!({"update":{"status":"completed","rawOutput":{"path":path,"file_path":path,"url":path,"input":{"files":[path]}}}})
        };
        let id = format!("nongrant-{index}");
        insert(&db, &id, value, false).await;
        sqlx::query("UPDATE messages SET type=? WHERE id=?")
            .bind(kind)
            .bind(&id)
            .execute(db.pool())
            .await
            .unwrap();
        let encoded = path.bytes().map(|byte| format!("%{byte:02X}")).collect::<String>();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/public/shares/current/resources?path={encoded}"))
                    .header("x-share-token", &token)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
