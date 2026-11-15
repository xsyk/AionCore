use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::sync::Arc;

use aionui_api_types::AgentEnvEntry;
use aionui_db::{SqliteConversationRepository, init_database_memory, models::ConversationRow};
use serde_json::{Value, json};

const USER: &str = "system_default_user";
const ID: &str = "qa_short_1";

fn mode(path: &Path, permissions: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(permissions)).unwrap();
}

fn replace(path: &Path, bytes: &[u8], permissions: u32) {
    if path.exists() {
        mode(path, 0o600);
    }
    std::fs::write(path, bytes).unwrap();
    mode(path, permissions);
}

struct Fixture {
    root: tempfile::TempDir,
    scope_file: PathBuf,
    dir: PathBuf,
    binding: Value,
    id: String,
}

impl Fixture {
    fn new() -> Self {
        Self::for_id(ID)
    }

    fn for_id(id: &str) -> Self {
        // The EACCES test must run under the actual unprivileged service identity.
        assert_ne!(unsafe { libc::geteuid() }, 0, "run QA tests as the service account");
        let root = tempfile::tempdir().unwrap();
        let etc = root.path().join("etc/aioneasiful");
        let dir = etc.join("qa-session-launch");
        std::fs::create_dir_all(dir.join("driver")).unwrap();
        std::fs::create_dir_all(root.path().join("private")).unwrap();
        let binary = root.path().join("private/codex-qa");
        replace(&binary, b"\x7fELFtest-only", 0o700);
        replace(&dir.join("driver/launch_qa.py"), b"#!/usr/bin/python3\n", 0o555);
        replace(&dir.join("driver/probe.py"), b"# test-only probe\n", 0o555);
        let scope_file = etc.join("qa-session-scope.json");
        replace(
            &scope_file,
            &serde_json::to_vec(&json!({
                "version": 1, "target": {"conversation_id": id, "user_id": USER, "backend": "codex"}
            }))
            .unwrap(),
            0o444,
        );
        let binding = json!({
            "version": 1, "conversation_id": id, "user_id": USER, "backend": "codex",
            "expires_at": unix_now().unwrap() + 3600,
            "env": {
                "AION_QA_CODEX_PROJECTION_BINARY": binary.to_str().unwrap(),
                "AION_QA_CODEX_PROJECTION_EXPECT_EXE_SHA256": "a".repeat(64),
                "AION_QA_CODEX_PROJECTION_DIRECTORY": root.path().join("private").to_str().unwrap(),
                "AION_QA_CODEX_PROJECTION_NONCE_FILE": root.path().join("private/nonce").to_str().unwrap()
            }
        });
        let fixture = Self {
            root,
            scope_file,
            dir,
            binding,
            id: id.into(),
        };
        fixture.save_binding();
        fixture
    }

    fn path(&self) -> PathBuf {
        self.dir.join(format!("{}.json", self.id))
    }

    fn files(&self) -> TrustedFiles {
        TrustedFiles::for_test(self.root.path().to_path_buf(), unsafe { libc::geteuid() })
    }

    fn load(&self) -> Result<Policy> {
        Policy::load(self.files(), &self.scope_file, self.dir.clone())
    }

    fn save_binding(&self) {
        replace(&self.path(), &serde_json::to_vec(&self.binding).unwrap(), 0o444);
    }

    fn scope(&self, scope: Value) {
        replace(&self.scope_file, &serde_json::to_vec(&scope).unwrap(), 0o444);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Permit TempDir to clean up after permission-denial fixtures.
        mode(&self.dir, 0o755);
        mode(&self.dir.join("driver"), 0o755);
    }
}

fn expect_code<T>(result: Result<T>, code: &str) {
    match result {
        Err(error) => assert_eq!(error.0, code),
        Ok(_) => panic!("expected static QA rejection {code}"),
    }
}

async fn repository(ids: &[&str]) -> (aionui_db::Database, SqliteConversationRepository) {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteConversationRepository::new(db.pool().clone());
    for id in ids {
        repo.create(&ConversationRow {
            id: (*id).into(),
            user_id: USER.into(),
            name: "QA test".into(),
            r#type: "acp".into(),
            extra: "{}".into(),
            model: None,
            status: Some("pending".into()),
            source: None,
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
    (db, repo)
}

async fn resolve(policy: &Policy, repo: &dyn IConversationRepository) -> Result<Option<ValidatedQaLaunch>> {
    policy
        .resolve(
            "codex",
            ID,
            USER,
            Some(repo),
            QaLaunchEnvironment {
                metadata_env: &[],
                runtime_env: &[],
            },
            false,
        )
        .await
}

#[test]
fn qa_session_launch_invalid_scope_is_startup_configuration_error() {
    let fixture = Fixture::new();
    std::fs::remove_file(&fixture.scope_file).unwrap();
    expect_code(fixture.load(), "QA_SCOPE_CONFIG_INVALID");
    replace(&fixture.scope_file, b"secret-invalid-json", 0o444);
    expect_code(fixture.load(), "QA_SCOPE_CONFIG_INVALID");
    mode(&fixture.scope_file, 0o000);
    expect_code(fixture.load(), "QA_SCOPE_CONFIG_INVALID");
}

#[test]
fn qa_session_launch_scope_structure_is_exact_and_bounded() {
    let fixture = Fixture::new();
    for scope in [
        json!({"version": 2, "target": {"conversation_id": ID, "user_id": USER, "backend": "codex"}}),
        json!({"version": 1, "target": {"conversation_id": ID, "user_id": USER, "backend": "claude"}}),
        json!({"version": 1, "target": {"conversation_id": ID, "user_id": USER, "backend": "codex", "extra": true}}),
        json!({"version": 1, "target": {"conversation_id": ID, "user_id": USER, "backend": "codex"}, "targets": []}),
        json!({"version": 1, "target": [{"conversation_id": ID, "user_id": USER, "backend": "codex"}]}),
    ] {
        fixture.scope(scope);
        expect_code(fixture.load(), "QA_SCOPE_CONFIG_INVALID");
    }
    replace(&fixture.scope_file, br#"{"version":1,"version":1,"target":{"conversation_id":"qa_short_1","user_id":"system_default_user","backend":"codex"}}"#, 0o444);
    expect_code(fixture.load(), "QA_SCOPE_CONFIG_INVALID");
    replace(&fixture.scope_file, br#"{"version":1,"target":{"conversation_id":"qa_short_1","conversation_id":"other","user_id":"system_default_user","backend":"codex"}}"#, 0o444);
    expect_code(fixture.load(), "QA_SCOPE_CONFIG_INVALID");
    replace(&fixture.scope_file, &vec![b' '; SCOPE_LIMIT + 1], 0o444);
    expect_code(fixture.load(), "QA_SCOPE_CONFIG_INVALID");
}

#[test]
fn qa_session_launch_scope_rejects_filename_and_owner_attacks() {
    let fixture = Fixture::new();
    for bad in ["", ".", "..", "../qa", "a/b", "a\\b", "a.b", "prefix*", "a\0b", "用户"] {
        for field in ["conversation_id", "user_id"] {
            let mut scope =
                json!({"version": 1, "target": {"conversation_id": ID, "user_id": USER, "backend": "codex"}});
            scope["target"][field] = json!(bad);
            fixture.scope(scope);
            expect_code(fixture.load(), "QA_SCOPE_CONFIG_INVALID");
        }
    }
    assert!(!valid_id(&"x".repeat(129)));
    assert!(valid_id("conv_short-123"));
}

#[tokio::test]
async fn qa_session_launch_exact_short_id_and_persistent_owner_match() {
    let fixture = Fixture::new();
    let (_db, repo) = repository(&[ID]).await;
    let policy = fixture.load().unwrap();
    let launch = resolve(&policy, &repo).await.unwrap().unwrap();
    assert_eq!(launch.conversation_id, ID);
    assert_eq!(launch.user_id, USER);
    assert_eq!(launch.program, fixture.dir.join("driver/launch_qa.py"));
    assert_eq!(launch.env.len(), 4);
}

#[tokio::test]
async fn qa_session_launch_eacces_rejects_target_and_bypasses_non_target_directory() {
    let fixture = Fixture::new();
    let (_db, repo) = repository(&[ID]).await;
    let policy = fixture.load().unwrap();
    mode(&fixture.dir, 0o000);
    assert_eq!(
        std::fs::metadata(fixture.path()).unwrap_err().raw_os_error(),
        Some(libc::EACCES)
    );
    expect_code(resolve(&policy, &repo).await, "QA_BINDING_UNAVAILABLE");
    let opens = policy.files.open_count();
    // The counter proves these requests perform no trusted-file access, even
    // when the binding directory cannot be traversed. No DB is supplied either.
    for id in ["qa_short", "qa_short_1_fork", "another", "../ordinary"] {
        assert!(
            policy
                .resolve(
                    "codex",
                    id,
                    USER,
                    None,
                    QaLaunchEnvironment {
                        metadata_env: &[],
                        runtime_env: &[]
                    },
                    false
                )
                .await
                .unwrap()
                .is_none()
        );
    }
    assert!(
        policy
            .resolve(
                "claude",
                ID,
                USER,
                None,
                QaLaunchEnvironment {
                    metadata_env: &[],
                    runtime_env: &[]
                },
                false
            )
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        policy
            .resolve(
                "antigravity",
                ID,
                USER,
                None,
                QaLaunchEnvironment {
                    metadata_env: &[],
                    runtime_env: &[]
                },
                false
            )
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(policy.files.open_count(), opens);
}

#[tokio::test]
async fn qa_session_launch_deleted_binding_remains_in_cached_scope() {
    let fixture = Fixture::new();
    let (_db, repo) = repository(&[ID]).await;
    let policy = fixture.load().unwrap();
    assert!(resolve(&policy, &repo).await.unwrap().is_some());
    std::fs::remove_file(fixture.path()).unwrap();
    std::fs::remove_file(&fixture.scope_file).unwrap();
    expect_code(resolve(&policy, &repo).await, "QA_BINDING_UNAVAILABLE");
    assert!(
        policy
            .resolve(
                "codex",
                "other",
                USER,
                None,
                QaLaunchEnvironment {
                    metadata_env: &[],
                    runtime_env: &[]
                },
                false
            )
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn qa_session_launch_scope_changes_do_not_reload_membership() {
    let fixture = Fixture::new();
    let (_db, repo) = repository(&[ID]).await;
    let policy = fixture.load().unwrap();
    fixture
        .scope(json!({"version": 1, "target": {"conversation_id": "new_target", "user_id": USER, "backend": "codex"}}));
    assert!(resolve(&policy, &repo).await.unwrap().is_some());
    assert!(
        policy
            .resolve(
                "codex",
                "new_target",
                USER,
                None,
                QaLaunchEnvironment {
                    metadata_env: &[],
                    runtime_env: &[]
                },
                false
            )
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn qa_session_launch_context_db_and_scope_owners_must_all_match() {
    let fixture = Fixture::new();
    let (db, repo) = repository(&[ID]).await;
    let policy = fixture.load().unwrap();
    expect_code(
        policy
            .resolve(
                "codex",
                ID,
                "other",
                Some(&repo),
                QaLaunchEnvironment {
                    metadata_env: &[],
                    runtime_env: &[],
                },
                false,
            )
            .await,
        "QA_OWNER_MISMATCH",
    );
    expect_code(
        policy
            .resolve(
                "codex",
                ID,
                USER,
                None,
                QaLaunchEnvironment {
                    metadata_env: &[],
                    runtime_env: &[],
                },
                false,
            )
            .await,
        "QA_OWNER_UNAVAILABLE",
    );
    expect_code(
        policy
            .resolve(
                "codex",
                ID,
                USER,
                Some(&repo),
                QaLaunchEnvironment {
                    metadata_env: &[],
                    runtime_env: &[],
                },
                true,
            )
            .await,
        "QA_FORK_REJECTED",
    );
    repo.delete(USER, ID).await.unwrap();
    expect_code(resolve(&policy, &repo).await, "QA_OWNER_MISMATCH");
    sqlx::query("INSERT INTO users (id, username, password_hash, created_at, updated_at) VALUES ('other', 'other', 'hash', 1, 1)")
        .execute(db.pool()).await.unwrap();
    let mut row = ConversationRow {
        id: ID.into(),
        user_id: "other".into(),
        name: "other".into(),
        r#type: "acp".into(),
        extra: "{}".into(),
        model: None,
        status: Some("pending".into()),
        source: None,
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
    expect_code(resolve(&policy, &repo).await, "QA_OWNER_MISMATCH");
    row.id = "same_owner_different_id".into();
    row.user_id = USER.into();
    repo.create(&row).await.unwrap();
    expect_code(resolve(&policy, &repo).await, "QA_OWNER_MISMATCH");
}

#[tokio::test]
async fn qa_session_launch_binding_identity_and_backend_are_exact() {
    let mut fixture = Fixture::new();
    let (_db, repo) = repository(&[ID]).await;
    let policy = fixture.load().unwrap();
    let original = fixture.binding.clone();
    for (field, value, code) in [
        ("version", json!(2), "QA_BINDING_IDENTITY_MISMATCH"),
        ("conversation_id", json!("qa_short"), "QA_BINDING_IDENTITY_MISMATCH"),
        ("user_id", json!("other"), "QA_BINDING_IDENTITY_MISMATCH"),
        ("backend", json!("claude"), "QA_BINDING_INVALID"),
    ] {
        fixture.binding = original.clone();
        fixture.binding[field] = value;
        fixture.save_binding();
        expect_code(resolve(&policy, &repo).await, code);
    }
}

#[tokio::test]
async fn qa_session_launch_binding_requires_strict_structure_and_four_env_keys() {
    let mut fixture = Fixture::new();
    let (_db, repo) = repository(&[ID]).await;
    let policy = fixture.load().unwrap();
    let original = fixture.binding.clone();
    for variant in 0..4 {
        fixture.binding = original.clone();
        match variant {
            0 => {
                fixture.binding["unknown"] = json!("secret");
            }
            1 => {
                fixture.binding["env"]["EXTRA"] = json!("secret");
            }
            2 => {
                fixture.binding["env"]
                    .as_object_mut()
                    .unwrap()
                    .remove("AION_QA_CODEX_PROJECTION_NONCE_FILE");
            }
            _ => {
                fixture.binding["expires_at"] = json!(-1);
            }
        }
        fixture.save_binding();
        expect_code(resolve(&policy, &repo).await, "QA_BINDING_INVALID");
    }
    let raw = serde_json::to_string(&original)
        .unwrap()
        .replacen("{", "{\"version\":1,", 1);
    replace(&fixture.path(), raw.as_bytes(), 0o444);
    expect_code(resolve(&policy, &repo).await, "QA_BINDING_INVALID");
    let raw = serde_json::to_string(&original).unwrap().replace(
        "\"env\":{",
        "\"env\":{\"AION_QA_CODEX_PROJECTION_BINARY\":\"/duplicate\",",
    );
    replace(&fixture.path(), raw.as_bytes(), 0o444);
    expect_code(resolve(&policy, &repo).await, "QA_BINDING_INVALID");
}

#[tokio::test]
async fn qa_session_launch_expired_or_excessive_lifetime_never_falls_back() {
    let mut fixture = Fixture::new();
    let (_db, repo) = repository(&[ID]).await;
    let policy = fixture.load().unwrap();
    for expiration in [0, unix_now().unwrap(), unix_now().unwrap() + MAX_LIFETIME + 100] {
        fixture.binding["expires_at"] = json!(expiration);
        fixture.save_binding();
        expect_code(resolve(&policy, &repo).await, "QA_BINDING_EXPIRED");
    }
}

#[tokio::test]
async fn qa_session_launch_env_paths_and_sha_are_strict() {
    let mut fixture = Fixture::new();
    let (_db, repo) = repository(&[ID]).await;
    let policy = fixture.load().unwrap();
    let original = fixture.binding.clone();
    for field in [
        "AION_QA_CODEX_PROJECTION_BINARY",
        "AION_QA_CODEX_PROJECTION_DIRECTORY",
        "AION_QA_CODEX_PROJECTION_NONCE_FILE",
    ] {
        for bad in ["relative", "/", "/a/../b", "/a/./b", "/a//b", "/a/", "/a\0b"] {
            fixture.binding = original.clone();
            fixture.binding["env"][field] = json!(bad);
            fixture.save_binding();
            expect_code(resolve(&policy, &repo).await, "QA_BINDING_ENV_INVALID");
        }
    }
    for hash in ["A".repeat(64), "g".repeat(64), "a".repeat(63), "a".repeat(65)] {
        fixture.binding = original.clone();
        fixture.binding["env"]["AION_QA_CODEX_PROJECTION_EXPECT_EXE_SHA256"] = json!(hash);
        fixture.save_binding();
        expect_code(resolve(&policy, &repo).await, "QA_BINDING_ENV_INVALID");
    }
}

#[tokio::test]
async fn qa_session_launch_reserved_metadata_and_runtime_namespaces_are_rejected() {
    let fixture = Fixture::new();
    let (_db, repo) = repository(&[ID]).await;
    let policy = fixture.load().unwrap();
    for key in [
        "AION_QA_CODEX_PROJECTION_BINARY",
        "AION_QA_CODEX_PROJECTION_EXPECT_EXE_SHA256",
        "AION_QA_CODEX_PROJECTION_DIRECTORY",
        "AION_QA_CODEX_PROJECTION_NONCE_FILE",
        "AION_QA_OTHER",
        "aion_qa_hidden",
        "AIONUI_QA_OTHER",
    ] {
        let metadata = [AgentEnvEntry {
            name: key.into(),
            value: "secret".into(),
            description: None,
        }];
        expect_code(
            policy
                .resolve(
                    "codex",
                    ID,
                    USER,
                    Some(&repo),
                    QaLaunchEnvironment {
                        metadata_env: &metadata,
                        runtime_env: &[],
                    },
                    false,
                )
                .await,
            "QA_RESERVED_ENV_CONFLICT",
        );
        let runtime = [(key.into(), "secret".into())];
        expect_code(
            policy
                .resolve(
                    "codex",
                    ID,
                    USER,
                    Some(&repo),
                    QaLaunchEnvironment {
                        metadata_env: &[],
                        runtime_env: &runtime,
                    },
                    false,
                )
                .await,
            "QA_RESERVED_ENV_CONFLICT",
        );
    }
}

#[tokio::test]
async fn qa_session_launch_untrusted_binding_files_fail_only_the_target() {
    let fixture = Fixture::new();
    let (_db, repo) = repository(&[ID]).await;
    let policy = fixture.load().unwrap();
    mode(&fixture.path(), 0o666);
    expect_code(resolve(&policy, &repo).await, "QA_BINDING_UNAVAILABLE");
    fixture.save_binding();
    std::fs::hard_link(fixture.path(), fixture.dir.join("hardlink")).unwrap();
    expect_code(resolve(&policy, &repo).await, "QA_BINDING_UNAVAILABLE");
    std::fs::remove_file(fixture.dir.join("hardlink")).unwrap();
    replace(&fixture.path(), &vec![b' '; BINDING_LIMIT + 1], 0o444);
    expect_code(resolve(&policy, &repo).await, "QA_BINDING_UNAVAILABLE");
    std::fs::remove_file(fixture.path()).unwrap();
    symlink(&fixture.scope_file, fixture.path()).unwrap();
    expect_code(resolve(&policy, &repo).await, "QA_BINDING_UNAVAILABLE");
    assert!(
        policy
            .resolve(
                "codex",
                "unrelated",
                USER,
                None,
                QaLaunchEnvironment {
                    metadata_env: &[],
                    runtime_env: &[]
                },
                false
            )
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn qa_session_launch_driver_and_probe_cannot_be_replaced_or_made_writable() {
    let fixture = Fixture::new();
    let (_db, repo) = repository(&[ID]).await;
    let policy = fixture.load().unwrap();
    for name in ["launch_qa.py", "probe.py"] {
        let path = fixture.dir.join("driver").join(name);
        mode(&path, 0o777);
        expect_code(resolve(&policy, &repo).await, "QA_DRIVER_REJECTED");
        mode(&path, 0o444);
        expect_code(resolve(&policy, &repo).await, "QA_DRIVER_REJECTED");
        std::fs::remove_file(&path).unwrap();
        symlink(&fixture.scope_file, &path).unwrap();
        expect_code(resolve(&policy, &repo).await, "QA_DRIVER_REJECTED");
        std::fs::remove_file(&path).unwrap();
        replace(&path, b"# test-only\n", 0o555);
    }
    mode(&fixture.dir.join("driver"), 0o777);
    expect_code(resolve(&policy, &repo).await, "QA_DRIVER_REJECTED");
}

#[tokio::test]
async fn qa_session_launch_service_owned_elf_is_private_and_single_linked() {
    let fixture = Fixture::new();
    let (_db, repo) = repository(&[ID]).await;
    let policy = fixture.load().unwrap();
    assert!(resolve(&policy, &repo).await.unwrap().is_some());
    let binary = Path::new(
        fixture.binding["env"]["AION_QA_CODEX_PROJECTION_BINARY"]
            .as_str()
            .unwrap(),
    );
    mode(binary, 0o755);
    expect_code(resolve(&policy, &repo).await, "QA_BINARY_REJECTED");
    mode(binary, 0o700);
    std::fs::hard_link(binary, fixture.root.path().join("private/hardlink")).unwrap();
    expect_code(resolve(&policy, &repo).await, "QA_BINARY_REJECTED");
    std::fs::remove_file(fixture.root.path().join("private/hardlink")).unwrap();
    replace(binary, b"not-an-elf", 0o700);
    expect_code(resolve(&policy, &repo).await, "QA_BINARY_REJECTED");
}

#[tokio::test]
async fn qa_session_launch_preserves_normal_config_and_disables_writeback_and_dumps() {
    let fixture = Fixture::new();
    let (db, repo) = repository(&[ID]).await;
    let policy = fixture.load().unwrap();
    let ordinary_env = [EnvVar {
        name: "KEEP".into(),
        value: "ordinary".into(),
    }];
    let mut config = aionui_session::SessionConfig {
        cwd: Some("/workspace".into()),
        model: Some("chosen-model".into()),
        mode: Some("chosen-mode".into()),
        sandbox_mode: Some("workspace-write".into()),
        approval_policy: Some("on-request".into()),
        extra_args: vec!["ordinary-arg".into()],
        cli_program: Some("/ordinary/codex".into()),
        spawn_env: ordinary_env.to_vec(),
        ..Default::default()
    };
    config.init.preset_context = Some("ordinary context".into());
    let ordinary = config.clone();
    let registry = crate::registry::AgentRegistry::new(Arc::new(aionui_db::SqliteAgentMetadataRepository::new(
        db.pool().clone(),
    )));
    let mut catalog = Some(("shared-row".into(), registry.catalog_sender()));
    let dump_path = fixture.root.path().join("must-not-exist");
    let mut dump = Some(dump_path.clone());
    let before_global = std::env::var_os("AION_QA_CODEX_PROJECTION_BINARY");
    resolve(&policy, &repo)
        .await
        .unwrap()
        .unwrap()
        .apply("codex", ID, USER, &mut config, &mut catalog, &mut dump)
        .unwrap();
    assert_eq!(config.cwd.as_deref(), Some("/workspace"));
    assert_eq!(config.model.as_deref(), Some("chosen-model"));
    assert_eq!(config.mode.as_deref(), Some("chosen-mode"));
    assert_eq!(config.sandbox_mode.as_deref(), Some("workspace-write"));
    assert_eq!(config.approval_policy.as_deref(), Some("on-request"));
    assert_eq!(config.extra_args, ["ordinary-arg"]);
    assert_eq!(config.init.preset_context.as_deref(), Some("ordinary context"));
    assert_eq!(config.cli_program, Some(fixture.dir.join("driver/launch_qa.py")));
    assert_eq!(config.spawn_env.len(), 5);
    assert_eq!(config.spawn_env[0].name, "KEEP");
    assert_eq!(config.spawn_env[0].value, "ordinary");
    for entry in &config.spawn_env[1..] {
        assert_eq!(fixture.binding["env"][&entry.name].as_str(), Some(entry.value.as_str()));
    }
    let names: std::collections::HashSet<_> = config.spawn_env[1..].iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(names.len(), 4);
    let mut preserved = config.clone();
    preserved.cli_program = ordinary.cli_program.clone();
    preserved.spawn_env = ordinary.spawn_env.clone();
    assert_eq!(preserved, ordinary, "all ordinary SessionConfig fields must survive");
    assert!(catalog.is_none());
    assert!(dump.is_none());
    assert!(!dump_path.exists());
    assert_eq!(std::env::var_os("AION_QA_CODEX_PROJECTION_BINARY"), before_global);
}

#[tokio::test]
async fn qa_session_launch_validated_value_cannot_cross_context_or_expire_during_build() {
    let fixture = Fixture::new();
    let (_db, repo) = repository(&[ID]).await;
    let policy = fixture.load().unwrap();
    for (backend, id, user) in [("claude", ID, USER), ("codex", "other", USER), ("codex", ID, "other")] {
        let mut config = aionui_session::SessionConfig::default();
        let error = resolve(&policy, &repo)
            .await
            .unwrap()
            .unwrap()
            .apply(backend, id, user, &mut config, &mut None, &mut None)
            .err()
            .unwrap();
        assert_eq!(error.to_string(), "Forbidden: QA_LAUNCH_CONTEXT_MISMATCH");
        assert!(config.cli_program.is_none());
    }
    let mut launch = resolve(&policy, &repo).await.unwrap().unwrap();
    launch.expires_at = 0;
    let error = launch
        .apply("codex", ID, USER, &mut Default::default(), &mut None, &mut None)
        .err()
        .unwrap();
    assert_eq!(error.to_string(), "Forbidden: QA_BINDING_EXPIRED");
}

#[tokio::test]
async fn qa_session_launch_final_spawn_env_conflict_is_rejected_before_mutation() {
    let fixture = Fixture::new();
    let (_db, repo) = repository(&[ID]).await;
    let policy = fixture.load().unwrap();
    let mut config = aionui_session::SessionConfig {
        cli_program: Some("ordinary".into()),
        ..Default::default()
    };
    config.spawn_env.push(EnvVar {
        name: "AION_QA_OTHER".into(),
        value: "secret".into(),
    });
    let mut dump = Some(PathBuf::from("/ordinary-dump"));
    let error = resolve(&policy, &repo)
        .await
        .unwrap()
        .unwrap()
        .apply("codex", ID, USER, &mut config, &mut None, &mut dump)
        .err()
        .unwrap();
    assert_eq!(error.to_string(), "Forbidden: QA_RESERVED_ENV_CONFLICT");
    assert_eq!(config.cli_program, Some(PathBuf::from("ordinary")));
    assert_eq!(config.spawn_env.len(), 1);
    assert_eq!(dump, Some(PathBuf::from("/ordinary-dump")));
}

#[tokio::test]
async fn qa_session_launch_concurrent_policies_and_ordinary_sessions_are_isolated() {
    let first = Fixture::for_id("qa_a");
    let second = Fixture::for_id("qa_b");
    let (_db, repo) = repository(&["qa_a", "qa_b"]).await;
    let a = first.load().unwrap();
    let b = second.load().unwrap();
    let jobs = (0..32).map(|index| {
        let (policy, id) = if index % 2 == 0 { (&a, "qa_a") } else { (&b, "qa_b") };
        let repo = &repo;
        let expected = if id == "qa_a" { &first.binding } else { &second.binding };
        async move {
            let launch = policy
                .resolve(
                    "codex",
                    id,
                    USER,
                    Some(repo),
                    QaLaunchEnvironment {
                        metadata_env: &[],
                        runtime_env: &[],
                    },
                    false,
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(launch.conversation_id, id);
            assert_eq!(
                launch.env[0].value,
                expected["env"]["AION_QA_CODEX_PROJECTION_BINARY"].as_str().unwrap()
            );
            assert!(
                policy
                    .resolve(
                        "codex",
                        "ordinary",
                        USER,
                        None,
                        QaLaunchEnvironment {
                            metadata_env: &[],
                            runtime_env: &[]
                        },
                        false
                    )
                    .await
                    .unwrap()
                    .is_none()
            );
        }
    });
    futures_util::future::join_all(jobs).await;
}

#[tokio::test]
async fn qa_session_launch_repository_failure_has_only_static_reason() {
    let fixture = Fixture::new();
    let (db, repo) = repository(&[ID]).await;
    let policy = fixture.load().unwrap();
    db.pool().close().await;
    expect_code(resolve(&policy, &repo).await, "QA_OWNER_UNAVAILABLE");
}

#[tokio::test]
async fn qa_session_launch_direct_cli_assembly_uses_validated_program_and_exact_env() {
    use aionui_api_types::{AgentHandshake, AgentMetadata, AgentSource, AgentSourceInfo, BehaviorPolicy};
    use aionui_common::AgentType;

    #[derive(Default)]
    struct Capture(std::sync::Mutex<Option<aionui_common::CommandSpec>>);
    #[async_trait::async_trait]
    impl aionui_process::Spawner for Capture {
        async fn spawn(
            &self,
            spec: aionui_common::CommandSpec,
            _extra_env: &[(String, String)],
            _owner: &str,
        ) -> std::result::Result<Arc<aionui_process::ManagedProcess>, aionui_process::ProcessError> {
            *self.0.lock().unwrap() = Some(spec);
            Err(aionui_process::ProcessError::internal(
                "QA test stops before process creation",
            ))
        }
    }
    let fixture = Fixture::new();
    let (_db, repo) = repository(&[ID]).await;
    let policy = fixture.load().unwrap();
    let metadata = AgentMetadata {
        id: "test".into(),
        icon: None,
        name: "Test".into(),
        name_i18n: None,
        description: None,
        description_i18n: None,
        backend: Some("codex".into()),
        agent_type: AgentType::Acp,
        agent_source: AgentSource::Builtin,
        agent_source_info: AgentSourceInfo::default(),
        enabled: true,
        available: true,
        command: None,
        resolved_command: None,
        args: vec![],
        env: vec![AgentEnvEntry {
            name: "KEEP".into(),
            value: "ordinary".into(),
            description: None,
        }],
        native_skills_dirs: None,
        skill_delivery: None,
        behavior_policy: BehaviorPolicy::default(),
        yolo_id: None,
        sort_order: 0,
        team_capable: false,
        last_check_status: None,
        last_check_kind: None,
        last_check_error_code: None,
        last_check_error_message: None,
        last_check_error_details: None,
        last_check_guidance: None,
        last_check_latency_ms: None,
        last_check_at: None,
        last_success_at: None,
        last_failure_at: None,
        handshake: AgentHandshake::default(),
        has_command_override: false,
        env_override_key_count: 0,
    };
    let config = aionui_api_types::AcpBuildExtra {
        backend: Some("codex".into()),
        ..Default::default()
    };
    let spawner = Arc::new(Capture::default());
    let dump_path = fixture.root.path().join("never-created-dump");
    let result = crate::session_agent::build_session_instance(
        "codex",
        crate::session_agent::SessionBuildInputs {
            conversation_id: ID.into(),
            user_id: USER.into(),
            workspace: fixture.root.path().to_str().unwrap().into(),
            config: &config,
            metadata: &metadata,
            skill_delivery: Default::default(),
            session_snapshot: None,
            backend_session_id: None,
            mcp_server_repo: None,
            runtime_env: &[],
            broadcaster: Arc::new(aionui_realtime::BroadcastEventBus::new(16)),
            catalog_writeback: None,
            acp_session_repo: None,
            prompt_dump_dir: Some(dump_path.clone()),
            permission_hook_body: None,
            qa_launch: resolve(&policy, &repo).await.unwrap(),
        },
        spawner.clone(),
    )
    .await;
    assert!(result.is_err(), "recording spawner must stop before creating a process");
    let command = spawner
        .0
        .lock()
        .unwrap()
        .clone()
        .expect("actual direct-CLI assembly must reach spawner");
    assert_eq!(command.command, fixture.dir.join("driver/launch_qa.py"));
    let qa_env: Vec<_> = command.env.iter().filter(|entry| reserved_key(&entry.name)).collect();
    assert_eq!(qa_env.len(), 4);
    for entry in qa_env {
        assert_eq!(fixture.binding["env"][&entry.name].as_str(), Some(entry.value.as_str()));
    }
    assert!(
        command
            .env
            .iter()
            .any(|entry| entry.name == "KEEP" && entry.value == "ordinary")
    );
    assert!(
        !dump_path.exists(),
        "QA config dump must be disabled before open_session"
    );
}

#[test]
fn qa_session_launch_scope_rejects_positional_array() {
    let fixture = Fixture::new();
    assert!(fixture.load().is_ok());
    for scope in [
        json!([1, {"conversation_id": ID, "user_id": USER, "backend": "codex"}]),
        json!([1, [ID, USER, "codex"]]),
    ] {
        fixture.scope(scope);
        expect_code(fixture.load(), "QA_SCOPE_CONFIG_INVALID");
    }
}

#[test]
fn qa_session_launch_target_rejects_positional_array() {
    let fixture = Fixture::new();
    assert!(fixture.load().is_ok());
    fixture.scope(json!({"version": 1, "target": [ID, USER, "codex"]}));
    expect_code(fixture.load(), "QA_SCOPE_CONFIG_INVALID");
}

#[test]
fn qa_session_launch_scope_backend_rejects_enum_object() {
    let fixture = Fixture::new();
    assert!(fixture.load().is_ok());
    fixture.scope(json!({
        "version": 1,
        "target": {"conversation_id": ID, "user_id": USER, "backend": {"codex": null}}
    }));
    expect_code(fixture.load(), "QA_SCOPE_CONFIG_INVALID");
}

#[tokio::test]
async fn qa_session_launch_binding_rejects_positional_array() {
    let mut fixture = Fixture::new();
    let (_db, repo) = repository(&[ID]).await;
    let policy = fixture.load().unwrap();
    assert!(resolve(&policy, &repo).await.unwrap().is_some());
    let original = fixture.binding.clone();
    fixture.binding = json!([
        original["version"],
        original["conversation_id"],
        original["user_id"],
        original["backend"],
        original["expires_at"],
        original["env"]
    ]);
    fixture.save_binding();
    expect_code(resolve(&policy, &repo).await, "QA_BINDING_INVALID");
}

#[tokio::test]
async fn qa_session_launch_env_rejects_positional_array() {
    let mut fixture = Fixture::new();
    let (_db, repo) = repository(&[ID]).await;
    let policy = fixture.load().unwrap();
    assert!(resolve(&policy, &repo).await.unwrap().is_some());
    let env = &fixture.binding["env"];
    let positional = json!([
        env["AION_QA_CODEX_PROJECTION_BINARY"],
        env["AION_QA_CODEX_PROJECTION_EXPECT_EXE_SHA256"],
        env["AION_QA_CODEX_PROJECTION_DIRECTORY"],
        env["AION_QA_CODEX_PROJECTION_NONCE_FILE"]
    ]);
    fixture.binding["env"] = positional;
    fixture.save_binding();
    expect_code(resolve(&policy, &repo).await, "QA_BINDING_INVALID");
}

#[tokio::test]
async fn qa_session_launch_binding_backend_rejects_enum_object() {
    let mut fixture = Fixture::new();
    let (_db, repo) = repository(&[ID]).await;
    let policy = fixture.load().unwrap();
    assert!(resolve(&policy, &repo).await.unwrap().is_some());
    fixture.binding["backend"] = json!({"codex": null});
    fixture.save_binding();
    expect_code(resolve(&policy, &repo).await, "QA_BINDING_INVALID");
}

#[tokio::test]
async fn qa_session_launch_env_requires_each_named_key_once() {
    let fixture = Fixture::new();
    let (_db, repo) = repository(&[ID]).await;
    let policy = fixture.load().unwrap();
    assert!(resolve(&policy, &repo).await.unwrap().is_some());
    let original = fixture.binding.clone();
    let raw = serde_json::to_string(&original).unwrap();
    for key in [
        "AION_QA_CODEX_PROJECTION_BINARY",
        "AION_QA_CODEX_PROJECTION_EXPECT_EXE_SHA256",
        "AION_QA_CODEX_PROJECTION_DIRECTORY",
        "AION_QA_CODEX_PROJECTION_NONCE_FILE",
    ] {
        let mut missing = original.clone();
        missing["env"].as_object_mut().unwrap().remove(key).unwrap();
        replace(&fixture.path(), &serde_json::to_vec(&missing).unwrap(), 0o444);
        expect_code(resolve(&policy, &repo).await, "QA_BINDING_INVALID");
        // Insert into raw JSON so a Value map cannot discard the duplicate.
        let member = format!("{}:{},", serde_json::to_string(key).unwrap(), original["env"][key]);
        let duplicate = raw.replace("\"env\":{", &format!("\"env\":{{{member}"));
        assert!(duplicate != raw, "raw duplicate insertion must change the document");
        replace(&fixture.path(), duplicate.as_bytes(), 0o444);
        expect_code(resolve(&policy, &repo).await, "QA_BINDING_INVALID");
    }
    fixture.save_binding();
    assert!(resolve(&policy, &repo).await.unwrap().is_some());
}

#[tokio::test]
async fn qa_session_launch_json_objects_preserve_document_boundary() {
    let fixture = Fixture::new();
    let (_db, repo) = repository(&[ID]).await;
    let scope = std::fs::read(&fixture.scope_file).unwrap();
    let binding = serde_json::to_vec(&fixture.binding).unwrap();
    let padded_scope = [b" \n".as_slice(), &scope, b"\t "].concat();
    let padded_binding = [b" \n".as_slice(), &binding, b"\t "].concat();
    replace(&fixture.scope_file, &padded_scope, 0o444);
    replace(&fixture.path(), &padded_binding, 0o444);
    let policy = fixture.load().unwrap();
    assert!(resolve(&policy, &repo).await.unwrap().is_some());
    for suffix in [b" {}".as_slice(), b" []".as_slice()] {
        replace(&fixture.scope_file, &[scope.as_slice(), suffix].concat(), 0o444);
        expect_code(fixture.load(), "QA_SCOPE_CONFIG_INVALID");
        replace(&fixture.path(), &[binding.as_slice(), suffix].concat(), 0o444);
        expect_code(resolve(&policy, &repo).await, "QA_BINDING_INVALID");
    }
}
