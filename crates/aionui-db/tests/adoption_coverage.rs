//! AionPro adoption coverage gate.
//!
//! `adopt_system_default_data` re-owns the local default user's data to the
//! machine's first external (AionPro) account by convention: any table with a
//! `user_id` or `owner_user_id` column is an ownership root and gets adopted.
//! A table whose ownership column uses any OTHER name would be silently
//! skipped — no error, no log (this is exactly how `project_explorer` was
//! missed before `owner_user_id` support landed).
//!
//! This test turns that silent gap into a hard gate: EVERY table in the live
//! schema must fall into exactly one declared category. A migration adding an
//! unclassified table (including ones merged in from main) fails this test
//! until someone makes a conscious adoption decision for it.

use aionui_db::init_database_memory;

/// Ownership follows the parent chain — the row's user is derived by joining
/// through the declared parent column to an owned root, so adoption of the
/// root covers these rows. Each entry is (table, parent column, owned root).
const PARENT_SCOPED: &[(&str, &str, &str)] = &[
    ("acp_session", "conversation_id", "conversations"),
    ("conversation_artifacts", "conversation_id", "conversations"),
    ("conversation_assistant_snapshots", "conversation_id", "conversations"),
    ("conversation_shares", "conversation_id", "conversations"),
    // `cron_job_runs.owner_id` is the scheduler's run-lease holder, NOT a
    // user reference — ownership flows from `job_id`.
    ("cron_job_runs", "job_id", "cron_jobs"),
    (
        "feishu_binding_baseline_messages",
        "binding_id",
        "feishu_conversation_bindings",
    ),
    ("feishu_inbox", "binding_id", "feishu_conversation_bindings"),
    ("feishu_outbox", "binding_id", "feishu_conversation_bindings"),
    ("feishu_gateway_baseline_messages", "route_id", "feishu_gateway_routes"),
    // Delivery audit rows intentionally outlive bindings, and a prepare can
    // precede its local route projection. Their owner is still the route's
    // conversation owner; serialized remote identities are never Core IDs.
    ("feishu_gateway_deliveries", "route_id", "feishu_gateway_routes"),
    // The relational owner follows the binding. Its serialized binding has
    // additional Core-owner aliases, explicitly rekeyed and tested below.
    (
        "feishu_gateway_interactions",
        "binding_id",
        "feishu_conversation_bindings",
    ),
    ("mailbox", "team_id", "teams"),
    ("messages", "conversation_id", "conversations"),
    ("team_tasks", "team_id", "teams"),
];

/// Deliberately machine-global tables — adoption must NOT touch them.
/// Every entry needs a rationale.
const GLOBAL_TABLES: &[(&str, &str)] = &[
    // Canonical filesystem-path registry shared by all users by design
    // (project-bind: folders are reused globally by resource_canonical).
    ("folders", "global canonical path registry"),
    // Single-row Feishu web-login settings owned by the super admin, not a user.
    ("feishu_login_config", "machine-wide feishu login settings"),
    // Login transactions bind a browser to a gateway node before any Core
    // account is resolved. Adopting user data must not alter or revive them.
    (
        "feishu_identity_transactions",
        "machine browser/node login transactions, no Core-user ownership",
    ),
    // Only immutable app/message replay keys; no user identity or message body.
    ("feishu_message_receipts", "app-scoped transport deduplication registry"),
    // This is the machine's node/app/tenant credential pin, with no Core-user
    // principal or message content. Adopting a user must not re-register a node.
    (
        "feishu_gateway_identity",
        "machine node/application/tenant identity pin",
    ),
    // Server-wide settings managed by the super admin (e.g. the image
    // generation model), shared by every user rather than owned by one.
    ("global_settings", "server-wide settings, not owned by any user"),
];

/// These tables carry a Core owner inside a serialized transport envelope,
/// before a relational parent can exist (/bind creates that parent). Adoption
/// must explicitly rekey this identity alongside its plugin. They are neither
/// machine-global data nor automatically covered by the column-name sweep.
const EMBEDDED_CORE_OWNER_TABLES: &[(&str, &str, &str)] =
    &[("feishu_ingress_events", "payload", "$.event.owner_user_id")];

/// Tables whose `user_id` column is NOT a Core-user ownership column — it
/// foreign-keys a non-Core identity table (a channel/platform user). The row's
/// Core owner is reached by joining that parent to its adopted root, so the
/// misleading `user_id` must be excluded from the ownership-root heuristic.
/// Without this, "has a `user_id` column" would classify the table as an
/// ownership root by accident and the gate would pass without a conscious
/// decision. Each entry: (table, the non-Core table its `user_id` points at).
const NON_CORE_USER_ID_TABLES: &[(&str, &str)] = &[
    // assistant_sessions.user_id -> assistant_users.id (a channel/platform
    // user). Core ownership flows through assistant_users.owner_user_id, which
    // adoption re-owns; adoption's UPDATE on this `user_id` never matches
    // 'system_default_user' (it holds assistant_users UUIDs), so it is a
    // harmless no-op.
    ("assistant_sessions", "assistant_users"),
];

/// Identity / infrastructure tables outside the ownership model.
const INFRA_TABLES: &[&str] = &["users", "_sqlx_migrations"];

async fn table_names(pool: &sqlx::SqlitePool) -> Vec<String> {
    sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn has_column(pool: &sqlx::SqlitePool, table: &str, column: &str) -> bool {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_table_info(?) WHERE name = ?")
        .bind(table)
        .bind(column)
        .fetch_one(pool)
        .await
        .unwrap();
    count > 0
}

#[tokio::test]
async fn every_table_is_classified_for_aionpro_adoption() {
    let db = init_database_memory().await.unwrap();
    let pool = db.pool();

    let mut unclassified = Vec::new();
    let mut overlaps = Vec::new();

    for table in table_names(pool).await {
        let non_core_user_id = NON_CORE_USER_ID_TABLES.iter().any(|(name, _)| *name == table);
        let owned_by_user_id = has_column(pool, &table, "user_id").await && table != "users" && !non_core_user_id;
        let owned_by_owner_user_id = has_column(pool, &table, "owner_user_id").await;
        let parent_scoped = PARENT_SCOPED.iter().any(|(name, _, _)| *name == table);
        let global = GLOBAL_TABLES.iter().any(|(name, _)| *name == table);
        let infra = INFRA_TABLES.contains(&table.as_str());
        let embedded_core_owner = EMBEDDED_CORE_OWNER_TABLES.iter().any(|(name, _, _)| *name == table);

        let categories = [
            owned_by_user_id,
            owned_by_owner_user_id,
            parent_scoped,
            global,
            infra,
            non_core_user_id,
            embedded_core_owner,
        ]
        .iter()
        .filter(|hit| **hit)
        .count();

        match categories {
            0 => unclassified.push(table),
            1 => {}
            _ => overlaps.push(table),
        }
    }

    assert!(
        overlaps.is_empty(),
        "tables classified in more than one adoption category (fix the declarations): {overlaps:?}"
    );
    assert!(
        unclassified.is_empty(),
        "NEW TABLE(S) WITHOUT AN AIONPRO ADOPTION DECISION: {unclassified:?}\n\
         Every table must either carry an ownership column (`user_id` / `owner_user_id`,\n\
         adopted automatically), or be declared in this test as parent-scoped (with its\n\
         parent column), explicitly handled embedded owner, machine-global (with a rationale), or infrastructure.\n\
         See crates/aionui-db/tests/adoption_coverage.rs."
    );

    db.close().await;
}

#[tokio::test]
async fn parent_scoped_declarations_match_the_live_schema() {
    let db = init_database_memory().await.unwrap();
    let pool = db.pool();
    let tables = table_names(pool).await;

    for (table, parent_column, owned_root) in PARENT_SCOPED {
        assert!(
            tables.iter().any(|name| name == table),
            "parent-scoped declaration references missing table {table}"
        );
        assert!(
            has_column(pool, table, parent_column).await,
            "parent-scoped table {table} lost its declared parent column {parent_column}"
        );
        assert!(
            has_column(pool, owned_root, "user_id").await || has_column(pool, owned_root, "owner_user_id").await,
            "parent-scoped table {table} points at root {owned_root}, which is not user-owned"
        );
        // A parent-scoped table must NOT quietly grow its own ownership column
        // (that would double-classify it and hide it from this gate's intent).
        assert!(
            !has_column(pool, table, "user_id").await && !has_column(pool, table, "owner_user_id").await,
            "{table} now has an ownership column — reclassify it as an ownership root"
        );
    }

    for (table, _) in GLOBAL_TABLES {
        assert!(
            tables.iter().any(|name| name == table),
            "global-table declaration references missing table {table}"
        );
    }

    for (table, column, path) in EMBEDDED_CORE_OWNER_TABLES {
        assert!(
            tables.iter().any(|name| name == table),
            "embedded-owner declaration references missing table {table}"
        );
        assert!(
            has_column(pool, table, column).await,
            "{table} lost its embedded-owner column {column}"
        );
        assert!(
            !path.is_empty(),
            "{table} must declare the exact embedded Core owner path"
        );
    }

    for (table, parent) in NON_CORE_USER_ID_TABLES {
        assert!(
            tables.iter().any(|name| name == table),
            "non-core-user-id declaration references missing table {table}"
        );
        assert!(
            has_column(pool, table, "user_id").await,
            "{table} was declared as carrying a non-core user_id but has no user_id column"
        );
        // The parent its user_id points at must itself be an adopted root —
        // owned via owner_user_id — so Core ownership genuinely flows through it.
        assert!(
            has_column(pool, parent, "owner_user_id").await,
            "{table}.user_id points at {parent}, which is not an owner_user_id root"
        );
    }

    db.close().await;
}

#[tokio::test]
async fn feishu_parent_and_connector_foreign_keys_match_the_ownership_model() {
    let db = init_database_memory().await.unwrap();
    for (table, column, parent) in [
        ("conversation_shares", "conversation_id", "conversations"),
        (
            "feishu_binding_baseline_messages",
            "binding_id",
            "feishu_conversation_bindings",
        ),
        ("feishu_inbox", "binding_id", "feishu_conversation_bindings"),
        ("feishu_outbox", "binding_id", "feishu_conversation_bindings"),
        ("feishu_conversation_bindings", "owner_user_id", "users"),
        ("feishu_conversation_bindings", "connector_user_id", "users"),
        ("feishu_conversation_binding_codes", "owner_user_id", "users"),
    ] {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_foreign_key_list(?) WHERE \"from\"=? AND \"table\"=? AND \"to\"='id'",
        )
        .bind(table)
        .bind(column)
        .bind(parent)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(count, 1, "{table}.{column} must reference {parent}.id");
    }
    db.close().await;
}

// ── upgrade adoption behavior ──────────────────────────────────────────

use aionui_db::{
    ExternalUserProjection, IProjectStore, IUserRepository, SqliteProjectStore, SqliteUserRepository, UserType,
};

async fn seed_default_user_project(pool: &sqlx::SqlitePool) -> (String, String) {
    let store = SqliteProjectStore::new(pool.clone());
    let folder = store
        .upsert_folder("file:///tmp/adopt-ws", "file:///tmp/adopt-ws")
        .await
        .unwrap();
    let (project, entry) = store
        .create_project_with_workspace_entry(
            "system_default_user",
            &folder.folder_id,
            "adopt-ws",
            aionui_db::ProjectKind::Standard,
        )
        .await
        .unwrap();
    (project.project_id, entry.pe_id)
}

#[tokio::test]
async fn first_external_user_adopts_owner_user_id_tables_too() {
    let db = init_database_memory().await.unwrap();
    let pool = db.pool();
    let repo = SqliteUserRepository::new(pool.clone());

    // Pre-upgrade machine data: a conversation (user_id table) and a
    // project + explorer workspace pair (user_id + owner_user_id tables).
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, status, created_at, updated_at)
         VALUES ('conv-legacy', 'system_default_user', 'Legacy', 'acp', '{}', 'pending', 1, 1)",
    )
    .execute(pool)
    .await
    .unwrap();
    let (project_id, pe_id) = seed_default_user_project(pool).await;

    // Channel binding owned by the pre-upgrade local user (`owner_user_id`
    // table with a coexisting platform identity column).
    sqlx::query(
        "INSERT INTO assistant_users (id, platform_user_id, platform_type, display_name, authorized_at, owner_user_id)
         VALUES ('au-legacy', 'tg-123', 'telegram', 'TG User', 1, 'system_default_user')",
    )
    .execute(pool)
    .await
    .unwrap();

    let user = repo
        .ensure_external_user(
            UserType::Aionpro,
            "ext-upgrade-1",
            ExternalUserProjection {
                username: Some("Pro".into()),
                email: None,
                avatar_path: None,
            },
        )
        .await
        .unwrap();
    let moved = repo.adopt_system_default_data(&user.id).await.unwrap();
    assert!(
        moved >= 4,
        "conversation + project + explorer entry + channel binding must move, moved={moved}"
    );

    let binding_owner: String = sqlx::query_scalar("SELECT owner_user_id FROM assistant_users WHERE id = 'au-legacy'")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(binding_owner, user.id, "channel platform binding must be adopted");

    let conv_owner: String = sqlx::query_scalar("SELECT user_id FROM conversations WHERE id = 'conv-legacy'")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(conv_owner, user.id);

    // The projects/project_explorer PAIR must move together — a split pair
    // (project adopted, explorer entry left behind) is exactly the bug this
    // extension fixes.
    let project_owner: String = sqlx::query_scalar("SELECT user_id FROM projects WHERE project_id = ?")
        .bind(&project_id)
        .fetch_one(pool)
        .await
        .unwrap();
    let entry_owner: String = sqlx::query_scalar("SELECT owner_user_id FROM project_explorer WHERE pe_id = ?")
        .bind(&pe_id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(project_owner, user.id);
    assert_eq!(entry_owner, user.id, "explorer entry must be adopted with its project");

    db.close().await;
}

#[tokio::test]
async fn adoption_window_closes_with_second_external_user() {
    let db = init_database_memory().await.unwrap();
    let pool = db.pool();
    let repo = SqliteUserRepository::new(pool.clone());
    let (project_id, _pe_id) = seed_default_user_project(pool).await;

    let first = repo
        .ensure_external_user(
            UserType::Aionpro,
            "ext-a",
            ExternalUserProjection {
                username: Some("A".into()),
                email: None,
                avatar_path: None,
            },
        )
        .await
        .unwrap();
    let second = repo
        .ensure_external_user(
            UserType::Aionpro,
            "ext-b",
            ExternalUserProjection {
                username: Some("B".into()),
                email: None,
                avatar_path: None,
            },
        )
        .await
        .unwrap();

    // Two external users exist: neither may adopt, in either order.
    assert_eq!(repo.adopt_system_default_data(&first.id).await.unwrap(), 0);
    assert_eq!(repo.adopt_system_default_data(&second.id).await.unwrap(), 0);

    let project_owner: String = sqlx::query_scalar("SELECT user_id FROM projects WHERE project_id = ?")
        .bind(&project_id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(project_owner, "system_default_user", "closed window must move nothing");

    db.close().await;
}

#[tokio::test]
async fn adoption_fires_once_then_new_local_data_stays_local() {
    let db = init_database_memory().await.unwrap();
    let pool = db.pool();
    let repo = SqliteUserRepository::new(pool.clone());

    // Pre-upgrade local data.
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, status, created_at, updated_at)
         VALUES ('conv-old', 'system_default_user', 'Old', 'acp', '{}', 'pending', 1, 1)",
    )
    .execute(pool)
    .await
    .unwrap();

    let user = repo
        .ensure_external_user(UserType::Aionpro, "ext-once", ExternalUserProjection::default())
        .await
        .unwrap();

    // First login: sweeps the old data and stamps the one-shot marker.
    assert!(repo.adopt_system_default_data(&user.id).await.unwrap() >= 1);
    let adopted_by: Option<String> =
        sqlx::query_scalar("SELECT adopted_by FROM users WHERE id = 'system_default_user'")
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(adopted_by.as_deref(), Some(user.id.as_str()), "marker must be stamped");
    assert!(repo.is_default_data_adopter(&user.id).await.unwrap());

    // AionUi keeps working locally: NEW data lands on the default user.
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, status, created_at, updated_at)
         VALUES ('conv-new', 'system_default_user', 'New', 'acp', '{}', 'pending', 2, 2)",
    )
    .execute(pool)
    .await
    .unwrap();

    // Later logins must NOT re-sweep it — the marker closes the window even
    // though the account is still the machine's sole external user.
    assert_eq!(repo.adopt_system_default_data(&user.id).await.unwrap(), 0);
    let owner: String = sqlx::query_scalar("SELECT user_id FROM conversations WHERE id = 'conv-new'")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(owner, "system_default_user", "post-adoption local data must stay local");

    db.close().await;
}

#[tokio::test]
async fn empty_first_sweep_still_consumes_the_adoption_opportunity() {
    let db = init_database_memory().await.unwrap();
    let pool = db.pool();
    let repo = SqliteUserRepository::new(pool.clone());

    // Fresh machine: nothing to adopt on first login.
    let user = repo
        .ensure_external_user(UserType::Aionpro, "ext-fresh", ExternalUserProjection::default())
        .await
        .unwrap();
    assert_eq!(repo.adopt_system_default_data(&user.id).await.unwrap(), 0);
    assert!(
        repo.is_default_data_adopter(&user.id).await.unwrap(),
        "a zero-row sweep still stamps the marker"
    );

    // Local data created afterwards stays local forever.
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, status, created_at, updated_at)
         VALUES ('conv-later', 'system_default_user', 'Later', 'acp', '{}', 'pending', 3, 3)",
    )
    .execute(pool)
    .await
    .unwrap();
    assert_eq!(repo.adopt_system_default_data(&user.id).await.unwrap(), 0);
    let owner: String = sqlx::query_scalar("SELECT user_id FROM conversations WHERE id = 'conv-later'")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(owner, "system_default_user");

    // The marker is account-specific: nobody else counts as the adopter.
    let other = repo
        .ensure_external_user(UserType::Aionpro, "ext-other", ExternalUserProjection::default())
        .await
        .unwrap();
    assert!(!repo.is_default_data_adopter(&other.id).await.unwrap());

    db.close().await;
}

async fn seed_feishu_adoption_graph(pool: &sqlx::SqlitePool) {
    sqlx::raw_sql(
        "INSERT INTO users(id,user_type,username,password_hash,created_at,updated_at) VALUES('other-local','local','other-local','hash',1,1);
         INSERT INTO conversations(id,user_id,name,type,created_at,updated_at) VALUES('adopt-shared','system_default_user','Shared','acp',1,1),('other-shared','other-local','Other','acp',1,1);
         INSERT INTO conversation_shares(conversation_id,token,enabled,created_at,updated_at) VALUES('adopt-shared','fixed-share-token',1,1,1);
         INSERT INTO assistant_plugins(id,owner_user_id,type,name,enabled,config,created_at,updated_at) VALUES('lark','system_default_user','lark','Bot',1,'sealed-original-config',1,1);
         INSERT INTO feishu_conversation_bindings(id,conversation_id,owner_user_id,connector_user_id,app_id,tenant_key,chat_id,chat_type,bound_at) VALUES('adopt-binding','adopt-shared','system_default_user','system_default_user','app','tenant','chat','group',1),('other-binding','other-shared','other-local','other-local','other-app','tenant','other-chat','group',1);
         INSERT INTO feishu_conversation_binding_codes(code,conversation_id,owner_user_id,expires_at) VALUES('pending-code','adopt-shared','system_default_user',9999);
         INSERT INTO feishu_binding_baseline_messages(binding_id,message_id) VALUES('adopt-binding','baseline-message');
         INSERT INTO feishu_inbox(id,binding_id,content) VALUES('input','adopt-binding','system_default_user is literal conversation text');
         INSERT INTO feishu_outbox(id,binding_id,message_id,part,content) VALUES('output','adopt-binding','reply',0,'system_default_user is literal reply text');
         INSERT INTO feishu_message_receipts(app_id,message_id,received_at) VALUES('app','receipt',1);"
    ).execute(pool).await.unwrap();
    for (id, owner) in [
        ("pending-connector", "system_default_user"),
        ("other-connector", "other-local"),
    ] {
        let payload = serde_json::json!({
            "event": {"id": id, "owner_user_id": owner, "app_id": "app", "bot_open_id": "bot", "frame_type": "event", "payload": "system_default_user is platform payload text"},
            "binding_generation": "adopt-binding"
        });
        sqlx::query("INSERT INTO feishu_ingress_events(id,payload) VALUES(?,?)")
            .bind(id)
            .bind(payload.to_string())
            .execute(pool)
            .await
            .unwrap();
    }
    // Old malformed/unscoped receipts are discarded by replay, so adoption
    // must preserve them rather than guessing at an unrelated identity field.
    sqlx::query("INSERT INTO feishu_ingress_events(id,payload) VALUES('malformed','not-json'),('legacy','{\"owner_user_id\":\"system_default_user\"}')")
        .execute(pool).await.unwrap();
}

#[tokio::test]
async fn adoption_keeps_feishu_connector_aliases_and_parent_scoped_queues_consistent() {
    let db = init_database_memory().await.unwrap();
    let pool = db.pool();
    let repo = SqliteUserRepository::new(pool.clone());
    seed_feishu_adoption_graph(pool).await;
    let user = repo
        .ensure_external_user(UserType::Aionpro, "feishu-adopter", ExternalUserProjection::default())
        .await
        .unwrap();
    repo.adopt_system_default_data(&user.id).await.unwrap();

    let owner: String = sqlx::query_scalar("SELECT owner_user_id FROM assistant_plugins WHERE id='lark'")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(owner, user.id);
    let binding: (String, String) = sqlx::query_as(
        "SELECT owner_user_id,connector_user_id FROM feishu_conversation_bindings WHERE id='adopt-binding'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(binding.0, user.id);
    assert_eq!(
        binding.1, user.id,
        "connector alias must follow the adopted plugin, or replies use a missing connector"
    );
    let other: (String, String) = sqlx::query_as(
        "SELECT owner_user_id,connector_user_id FROM feishu_conversation_bindings WHERE id='other-binding'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(other, ("other-local".into(), "other-local".into()));
    let code_owner: String =
        sqlx::query_scalar("SELECT owner_user_id FROM feishu_conversation_binding_codes WHERE code='pending-code'")
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(code_owner, user.id);
    let ingress: String = sqlx::query_scalar("SELECT payload FROM feishu_ingress_events WHERE id='pending-connector'")
        .fetch_one(pool)
        .await
        .unwrap();
    let ingress: serde_json::Value = serde_json::from_str(&ingress).unwrap();
    assert_eq!(ingress["event"]["owner_user_id"], user.id);
    assert_eq!(
        ingress["event"]["payload"],
        "system_default_user is platform payload text"
    );
    assert_eq!(ingress["binding_generation"], "adopt-binding");
    let other_ingress: String = sqlx::query_scalar(
        "SELECT json_extract(payload,'$.event.owner_user_id') FROM feishu_ingress_events WHERE id='other-connector'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(other_ingress, "other-local");
    let malformed: String = sqlx::query_scalar("SELECT payload FROM feishu_ingress_events WHERE id='malformed'")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(malformed, "not-json");
    let legacy: String = sqlx::query_scalar("SELECT payload FROM feishu_ingress_events WHERE id='legacy'")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(legacy, "{\"owner_user_id\":\"system_default_user\"}");

    let inherited: (String, String, String, String) = sqlx::query_as("SELECT c.user_id,s.token,i.content,o.content FROM conversations c JOIN conversation_shares s ON s.conversation_id=c.id JOIN feishu_conversation_bindings b ON b.conversation_id=c.id JOIN feishu_inbox i ON i.binding_id=b.id JOIN feishu_outbox o ON o.binding_id=b.id WHERE c.id='adopt-shared'").fetch_one(pool).await.unwrap();
    assert_eq!(
        inherited,
        (
            user.id.clone(),
            "fixed-share-token".into(),
            "system_default_user is literal conversation text".into(),
            "system_default_user is literal reply text".into()
        )
    );
    let baseline: String =
        sqlx::query_scalar("SELECT message_id FROM feishu_binding_baseline_messages WHERE binding_id='adopt-binding'")
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(baseline, "baseline-message");
    let receipts: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM feishu_message_receipts WHERE app_id='app' AND message_id='receipt'")
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(receipts, 1);
    assert_eq!(repo.adopt_system_default_data(&user.id).await.unwrap(), 0);
    db.close().await;
}

#[tokio::test]
async fn adoption_does_not_retarget_a_connector_when_its_plugin_was_not_adopted() {
    let db = init_database_memory().await.unwrap();
    let pool = db.pool();
    let repo = SqliteUserRepository::new(pool.clone());
    seed_feishu_adoption_graph(pool).await;
    let user = repo
        .ensure_external_user(UserType::Aionpro, "feishu-collision", ExternalUserProjection::default())
        .await
        .unwrap();
    // UPDATE OR IGNORE deliberately leaves the local connector in place when
    // the adopter already has a different Lark configuration under the same ID.
    sqlx::query("INSERT INTO assistant_plugins(id,owner_user_id,type,name,enabled,config,created_at,updated_at) VALUES('lark',?,'lark','Existing bot',1,'sealed-existing-config',1,1)")
        .bind(&user.id).execute(pool).await.unwrap();
    repo.adopt_system_default_data(&user.id).await.unwrap();
    let connector: String =
        sqlx::query_scalar("SELECT connector_user_id FROM feishu_conversation_bindings WHERE id='adopt-binding'")
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(
        connector, "system_default_user",
        "a retained local plugin must keep its actual connector identity"
    );
    let ingress_owner: String = sqlx::query_scalar(
        "SELECT json_extract(payload,'$.event.owner_user_id') FROM feishu_ingress_events WHERE id='pending-connector'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(ingress_owner, "system_default_user");
    let config: String = sqlx::query_scalar("SELECT config FROM assistant_plugins WHERE id='lark' AND owner_user_id=?")
        .bind(&user.id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(config, "sealed-existing-config");
    db.close().await;
}

async fn seed_gateway_adoption_graph(pool: &sqlx::SqlitePool) -> String {
    sqlx::raw_sql(
        "INSERT INTO conversations(id,user_id,name,type,created_at,updated_at) VALUES('gateway-shared','system_default_user','Gateway','acp',1,1),('gateway-other','other-local','Other gateway','acp',1,1),('gateway-prepared','system_default_user','Prepared gateway','acp',1,1);
         INSERT INTO feishu_gateway_identity(id,node_id,app_id,tenant_key) VALUES(1,'node-fixed','gateway-app','gateway-tenant');
         INSERT INTO feishu_gateway_routes(route_id,generation,node_id,conversation_id,owner_user_id,owner_union_id,app_id,tenant_key,chat_id,chat_type,status,prepared_at) VALUES('r-adopt','g-adopt','node-fixed','gateway-shared','system_default_user','platform-union-adopt','gateway-app','gateway-tenant','gateway-chat','group','active',1),('r-other','g-other','node-fixed','gateway-other','other-local','platform-union-other','gateway-app','gateway-tenant','gateway-other-chat','group','active',1),('r-prepared','g-prepared','node-fixed','gateway-prepared','system_default_user','platform-union-prepared','gateway-app','gateway-tenant','gateway-prepared-chat','group','prepared',1);
         INSERT INTO feishu_conversation_bindings(id,conversation_id,owner_user_id,connector_user_id,app_id,tenant_key,chat_id,chat_type,bound_at) VALUES('g-adopt','gateway-shared','system_default_user','system_default_user','gateway-app','gateway-tenant','gateway-chat','group',1),('g-other','gateway-other','other-local','other-local','gateway-app','gateway-tenant','gateway-other-chat','group',1);
         INSERT INTO feishu_gateway_baseline_messages(route_id,message_id) VALUES('r-adopt','system_default_user-is-a-message-id'),('r-prepared','prepared-baseline');"
    ).execute(pool).await.unwrap();
    let delivery = serde_json::json!({
        "id": "gateway-input", "kind": "message",
        "route": {"id": "r-adopt", "generation": "g-adopt", "owner_union_id": "platform-union-adopt"},
        "event": {"text": "system_default_user is literal platform text", "actor_union_id": "platform-participant"}
    })
    .to_string();
    sqlx::query("INSERT INTO feishu_gateway_deliveries(id,route_id,generation,kind,payload,attempt_epoch) VALUES('gateway-input','r-adopt','g-adopt','message',?,'original-epoch')")
        .bind(&delivery).execute(pool).await.unwrap();
    for (token, generation, conversation, owner, chat) in [
        (
            "gateway-card",
            "g-adopt",
            "gateway-shared",
            "system_default_user",
            "gateway-chat",
        ),
        (
            "other-gateway-card",
            "g-other",
            "gateway-other",
            "other-local",
            "gateway-other-chat",
        ),
    ] {
        let payload = serde_json::json!({
            "binding": {"id":generation,"conversation_id":conversation,"owner_user_id":owner,"connector_user_id":owner,"app_id":"gateway-app","tenant_key":"gateway-tenant","chat_id":chat,"chat_type":"group","bound_at":1},
            "request_id":"native-question", "msg_id":"message", "expires_at":9999,
            "kind":{"Ask":[{"text":"system_default_user is a literal question","options":["A","B"],"multiple":true,"selected":["A"]}]},
            "title":"system_default_user is literal card text", "message_id":"om-original"
        });
        sqlx::query("INSERT INTO feishu_gateway_interactions(token,binding_id,payload,expires_at) VALUES(?,?,?,9999)")
            .bind(token)
            .bind(generation)
            .bind(payload.to_string())
            .execute(pool)
            .await
            .unwrap();
    }
    let mismatched: String =
        sqlx::query_scalar("SELECT payload FROM feishu_gateway_interactions WHERE token='gateway-card'")
            .fetch_one(pool)
            .await
            .unwrap();
    let mut mismatched: serde_json::Value = serde_json::from_str(&mismatched).unwrap();
    mismatched["binding"]["id"] = serde_json::json!("g-other");
    sqlx::query("INSERT INTO feishu_gateway_interactions(token,binding_id,payload,expires_at) VALUES('mismatched-card','g-adopt',?,9999),('malformed-card','g-adopt','not-json',9999)")
        .bind(mismatched.to_string()).execute(pool).await.unwrap();
    delivery
}

#[tokio::test]
async fn gateway_adoption_rekeys_only_correlated_core_aliases_without_a_direct_lark_connector() {
    use aionui_db::{
        IFeishuBindingRepository, IFeishuGatewayRepository, SqliteFeishuBindingRepository,
        SqliteFeishuGatewayRepository,
    };

    // Gateway connector keys are local owners, independently of whether an
    // obsolete direct Lark plugin exists or can be adopted.
    for colliding_lark_plugin in [false, true] {
        let db = init_database_memory().await.unwrap();
        let pool = db.pool();
        seed_feishu_adoption_graph(pool).await;
        let original_delivery = seed_gateway_adoption_graph(pool).await;
        let repository = SqliteUserRepository::new(pool.clone());
        let user = repository
            .ensure_external_user(UserType::Aionpro, "gateway-adopter", ExternalUserProjection::default())
            .await
            .unwrap();
        if colliding_lark_plugin {
            sqlx::query("INSERT INTO assistant_plugins(id,owner_user_id,type,name,enabled,config,created_at,updated_at) VALUES('lark',?,'lark','Existing bot',1,'sealed-existing-config',1,1)")
                .bind(&user.id).execute(pool).await.unwrap();
        } else {
            sqlx::query("DELETE FROM assistant_plugins WHERE id='lark' AND owner_user_id='system_default_user'")
                .execute(pool)
                .await
                .unwrap();
        }
        let mismatched_before: String =
            sqlx::query_scalar("SELECT payload FROM feishu_gateway_interactions WHERE token='mismatched-card'")
                .fetch_one(pool)
                .await
                .unwrap();
        repository.adopt_system_default_data(&user.id).await.unwrap();
        let gateway = SqliteFeishuGatewayRepository::new(pool.clone());
        let bindings = SqliteFeishuBindingRepository::new(pool.clone());
        let adopted = gateway.route_by_generation("g-adopt").await.unwrap().unwrap();
        assert_eq!(adopted.owner_user_id, user.id);
        assert_eq!(adopted.owner_union_id, "platform-union-adopt");
        let binding = bindings
            .get_by_conversation(&user.id, "gateway-shared")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            binding.connector_user_id, user.id,
            "gateway aliases must move even if the old direct plugin cannot"
        );
        assert_eq!(
            gateway
                .route_by_generation("g-other")
                .await
                .unwrap()
                .unwrap()
                .owner_user_id,
            "other-local"
        );
        assert_eq!(
            bindings
                .get_by_conversation("other-local", "gateway-other")
                .await
                .unwrap()
                .unwrap()
                .connector_user_id,
            "other-local"
        );

        let snapshots = gateway.list_interactions(100).await.unwrap();
        let payload: serde_json::Value = serde_json::from_str(
            &snapshots
                .iter()
                .find(|row| row.token == "gateway-card")
                .unwrap()
                .payload,
        )
        .unwrap();
        assert_eq!(payload["binding"]["owner_user_id"], user.id);
        assert_eq!(payload["binding"]["connector_user_id"], user.id);
        assert_eq!(payload["binding"]["id"], "g-adopt");
        assert_eq!(payload["message_id"], "om-original");
        assert_eq!(payload["title"], "system_default_user is literal card text");
        assert_eq!(payload["kind"]["Ask"][0]["selected"], serde_json::json!(["A"]));
        let restored = bindings
            .get_by_conversation(
                payload["binding"]["owner_user_id"].as_str().unwrap(),
                payload["binding"]["conversation_id"].as_str().unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            restored.unwrap().id,
            "g-adopt",
            "the restored snapshot must resolve its adopted binding"
        );
        let other: serde_json::Value = serde_json::from_str(
            &snapshots
                .iter()
                .find(|row| row.token == "other-gateway-card")
                .unwrap()
                .payload,
        )
        .unwrap();
        assert_eq!(other["binding"]["owner_user_id"], "other-local");
        let malformed: String =
            sqlx::query_scalar("SELECT payload FROM feishu_gateway_interactions WHERE token='malformed-card'")
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(malformed, "not-json");
        let mismatched_after: String =
            sqlx::query_scalar("SELECT payload FROM feishu_gateway_interactions WHERE token='mismatched-card'")
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(
            mismatched_after, mismatched_before,
            "unrelated embedded binding identities must not be repaired into authority"
        );

        let receipt: (String, String, i64) =
            sqlx::query_as("SELECT payload,attempt_epoch,done FROM feishu_gateway_deliveries WHERE id='gateway-input'")
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(receipt, (original_delivery, "original-epoch".into(), 0));
        let parent_owner: String = sqlx::query_scalar("SELECT g.owner_user_id FROM feishu_gateway_deliveries d JOIN feishu_gateway_routes g ON g.route_id=d.route_id WHERE d.id='gateway-input'").fetch_one(pool).await.unwrap();
        assert_eq!(parent_owner, user.id);
        let baseline: (String, String) = sqlx::query_as("SELECT h.message_id,g.owner_user_id FROM feishu_gateway_baseline_messages h JOIN feishu_gateway_routes g ON g.route_id=h.route_id WHERE h.route_id='r-adopt'").fetch_one(pool).await.unwrap();
        assert_eq!(
            baseline,
            ("system_default_user-is-a-message-id".into(), user.id.clone())
        );
        let prepared = gateway.route_by_generation("g-prepared").await.unwrap().unwrap();
        assert_eq!(prepared.owner_user_id, user.id);
        assert_eq!(prepared.status, "prepared");
        let identity: (String, String, String) =
            sqlx::query_as("SELECT node_id,app_id,tenant_key FROM feishu_gateway_identity WHERE id=1")
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(
            identity,
            ("node-fixed".into(), "gateway-app".into(), "gateway-tenant".into())
        );
        let legacy_connector: String =
            sqlx::query_scalar("SELECT connector_user_id FROM feishu_conversation_bindings WHERE id='adopt-binding'")
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(
            legacy_connector, "system_default_user",
            "the unrelated unavailable direct connector retains its prior policy"
        );
        assert_eq!(repository.adopt_system_default_data(&user.id).await.unwrap(), 0);
        db.close().await;
    }
}
