//! SessionAgent's independent collaboration axis; these are adapter tests, not live CLI evidence.
use super::*;
use aionui_session::{Admission, Capabilities, CommandReceipt};
use futures_util::StreamExt;
use std::sync::Mutex;

struct ConfigBackend {
    caps: Mutex<Capabilities>,
    commands: Mutex<Vec<Command>>,
    reject: bool,
    unaccepted: bool,
    do_not_observe: bool,
    pending_rejections: std::sync::atomic::AtomicUsize,
    tx: broadcast::Sender<SessionEnvelope>,
}

impl ConfigBackend {
    fn new() -> Arc<Self> {
        let (tx, _) = broadcast::channel(32);
        Arc::new(Self {
            caps: Mutex::new(Capabilities {
                available_modes: vec![mode("on-request")],
                current_mode: Some("on-request".into()),
                available_models: vec![ModelInfo {
                    id: "confirmed-model".into(),
                    name: "Confirmed model".into(),
                    description: None,
                    reasoning_efforts: vec!["low".into(), "high".into()],
                }],
                current_model: Some("confirmed-model".into()),
                current_effort: Some("low".into()),
                available_collaboration_modes: vec![mode("plan"), mode("default")],
                ..Default::default()
            }),
            commands: Mutex::new(Vec::new()),
            reject: false,
            unaccepted: false,
            do_not_observe: false,
            pending_rejections: std::sync::atomic::AtomicUsize::new(0),
            tx,
        })
    }

    fn emit(&self, event: SessionEvent) {
        if let SessionEvent::CollaborationModeUpdated { modes, selected_mode } = &event {
            let mut caps = self.caps.lock().unwrap();
            caps.available_collaboration_modes = modes.clone();
            caps.current_collaboration_mode = selected_mode.clone();
        }
        self.tx
            .send(SessionEnvelope {
                session_id: "conv-1".into(),
                turn_gen: 0,
                event,
            })
            .unwrap();
    }
}

fn mode(id: &str) -> ModeInfo {
    ModeInfo {
        id: id.into(),
        name: id.into(),
        description: None,
    }
}

#[async_trait::async_trait]
impl SessionBackend for ConfigBackend {
    async fn dispatch(&self, command: Command) -> Result<CommandReceipt, BackendError> {
        self.commands.lock().unwrap().push(command.clone());
        if self.reject {
            return Err(BackendError::Transport("codex collaboration_mode busy".into()));
        }
        if self
            .pending_rejections
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(BackendError::Transport("codex settings pending confirmation".into()));
        }
        if !self.unaccepted && !self.do_not_observe {
            let mut caps = self.caps.lock().unwrap();
            if let Command::SetConfigOption { option_id, value } = command
                && option_id == "collaboration_mode"
            {
                caps.current_collaboration_mode = Some(value);
            }
        }
        Ok(CommandReceipt {
            accepted: !self.unaccepted,
            admission: Admission::NoTurn,
            turn_gen: 0,
        })
    }
    fn events(&self) -> BoxStream<'static, SessionEnvelope> {
        futures_util::stream::unfold(self.tx.subscribe(), |mut rx| async move {
            loop {
                match rx.recv().await {
                    Ok(event) => return Some((event, rx)),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        })
        .boxed()
    }
    fn capabilities(&self) -> Capabilities {
        self.caps.lock().unwrap().clone()
    }
}

fn task(backend: Arc<dyn SessionBackend>, repo: Option<Arc<dyn IAcpSessionRepository>>) -> Arc<SessionAgentTask> {
    SessionAgentTask::new(
        AgentType::Acp,
        "conv-1".into(),
        "user-1".into(),
        "/w".into(),
        backend,
        repo,
    )
}

fn assert_four(options: &[aionui_api_types::AcpConfigOptionDto], selected: Option<&str>) {
    assert_eq!(options.len(), 4, "every replacement snapshot must retain all four axes");
    for (id, value) in [
        ("mode", Some("on-request")),
        ("model", Some("confirmed-model")),
        ("reasoning_effort", Some("low")),
        ("collaboration_mode", selected),
    ] {
        let option = options
            .iter()
            .find(|o| o.id == id)
            .unwrap_or_else(|| panic!("missing {id}"));
        assert_eq!(option.current_value.as_deref(), value, "wrong highlight for {id}");
    }
    let option = options.iter().find(|o| o.id == "collaboration_mode").unwrap();
    assert_eq!(option.category.as_deref(), Some("collaboration_mode"));
    assert_eq!(option.option_type, "select");
    assert_eq!(
        option.options.iter().map(|v| v.value.as_str()).collect::<Vec<_>>(),
        ["plan", "default"]
    );
}

async fn pushed(rx: &mut broadcast::Receiver<AgentStreamEvent>) -> Vec<aionui_api_types::AcpConfigOptionDto> {
    let frame = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let AgentStreamEvent::AcpConfigOption(v) = rx.recv().await.unwrap() {
                break v;
            }
        }
    })
    .await
    .expect("config push must arrive without a turn");
    serde_json::from_value(frame["config_options"].clone()).unwrap()
}

#[tokio::test]
async fn collaboration_effort_confirmation_before_rest_pushes_high_then_null() {
    let backend = ConfigBackend::new();
    backend.caps.lock().unwrap().current_collaboration_mode = Some("plan".into());
    let task = task(backend.clone(), None);
    let mut rx = task.runtime.tx.subscribe();
    // Simulate a stale REST/restore optimistic choice; actual confirmation wins.
    task.runtime.set_effort_override("low".into());
    for confirmed in [Some("high"), None] {
        backend.emit(SessionEvent::EffortConfirmed {
            effort: confirmed.map(str::to_owned),
        });
        let options = pushed(&mut rx).await;
        assert_eq!(options.len(), 4);
        for (id, value) in [
            ("mode", Some("on-request")),
            ("model", Some("confirmed-model")),
            ("reasoning_effort", confirmed),
            ("collaboration_mode", Some("plan")),
        ] {
            assert_eq!(
                options.iter().find(|o| o.id == id).unwrap().current_value.as_deref(),
                value,
                "{id}"
            );
        }
        let caps = backend.capabilities();
        for event in [
            SessionEvent::CatalogUpdated {
                modes: caps.available_modes,
                models: caps.available_models,
                slash_commands: vec![],
            },
            SessionEvent::ConfigChanged {
                mode: Some("on-request".into()),
                model: Some("confirmed-model".into()),
            },
            SessionEvent::CollaborationModeUpdated {
                modes: vec![mode("plan"), mode("default")],
                selected_mode: Some("plan".into()),
            },
        ] {
            backend.emit(event);
            let next = pushed(&mut rx).await;
            assert_eq!(next.len(), 4);
            assert_eq!(
                next.iter()
                    .find(|o| o.id == "reasoning_effort")
                    .unwrap()
                    .current_value
                    .as_deref(),
                confirmed
            );
            assert_eq!(
                next.iter()
                    .find(|o| o.id == "collaboration_mode")
                    .unwrap()
                    .current_value
                    .as_deref(),
                Some("plan")
            );
        }
    }
}

#[tokio::test]
async fn collaboration_config_api_projects_independent_axis_and_none() {
    assert_four(
        &task(ConfigBackend::new(), None)
            .get_config_options()
            .await
            .unwrap()
            .config_options,
        None,
    );
}

#[tokio::test]
async fn collaboration_selection_observes_next_turn_token_with_four_axes() {
    let backend = ConfigBackend::new();
    let task = task(backend.clone(), None);
    for selected in ["plan", "default"] {
        let response = task.set_config_option("collaboration_mode", selected).await.unwrap();
        assert!(matches!(
            response.confirmation,
            aionui_api_types::ConfigOptionConfirmation::Observed
        ));
        assert_four(&response.config_options.unwrap(), Some(selected));
    }
    let commands = backend.commands.lock().unwrap();
    assert_eq!(commands.len(), 2);
    assert!(
        commands
            .iter()
            .all(|c| matches!(c, Command::SetConfigOption { option_id, .. } if option_id == "collaboration_mode"))
    );
}

#[tokio::test]
async fn collaboration_busy_error_preserves_selection() {
    let mut backend = ConfigBackend::new();
    Arc::get_mut(&mut backend).unwrap().reject = true;
    let task = task(backend, None);
    let error = task.set_config_option("collaboration_mode", "plan").await.unwrap_err();
    assert!(error.to_string().contains("collaboration_mode busy"));
    assert_four(&task.get_config_options().await.unwrap().config_options, None);
}

#[tokio::test]
async fn collaboration_unaccepted_receipt_is_not_success() {
    let mut backend = ConfigBackend::new();
    Arc::get_mut(&mut backend).unwrap().unaccepted = true;
    let task = task(backend, None);
    let error = task.set_config_option("collaboration_mode", "plan").await.unwrap_err();
    assert!(error.to_string().contains("not accepted"));
}

#[tokio::test]
async fn collaboration_runtime_event_and_sibling_pushes_keep_four_axes() {
    let backend = ConfigBackend::new();
    let task = task(backend.clone(), None);
    task.get_config_options().await.unwrap();
    let mut rx = task.runtime.tx.subscribe();
    backend.emit(SessionEvent::CollaborationModeUpdated {
        modes: vec![mode("plan"), mode("default")],
        selected_mode: Some("plan".into()),
    });
    assert_four(&pushed(&mut rx).await, Some("plan"));
    let caps = backend.capabilities();
    backend.emit(SessionEvent::CatalogUpdated {
        modes: caps.available_modes,
        models: caps.available_models,
        slash_commands: vec![],
    });
    assert_four(&pushed(&mut rx).await, Some("plan"));
    backend.emit(SessionEvent::ConfigChanged {
        mode: Some("on-request".into()),
        model: Some("confirmed-model".into()),
    });
    assert_four(&pushed(&mut rx).await, Some("plan"));
}

#[tokio::test]
async fn collaboration_non_codex_keeps_existing_three_axes() {
    let backend = ConfigBackend::new();
    backend.caps.lock().unwrap().available_collaboration_modes.clear();
    let task = task(backend, None);
    let snapshot = task.get_config_options().await.unwrap();
    assert_eq!(snapshot.config_options.len(), 3);
    assert!(snapshot.config_options.iter().all(|o| o.id != "collaboration_mode"));
    let response = task.set_config_option("reasoning_effort", "high").await.unwrap();
    assert!(matches!(
        response.confirmation,
        aionui_api_types::ConfigOptionConfirmation::Observed
    ));
    assert_eq!(response.config_options.unwrap().len(), 3);
}

fn persisted_selection(value: &str) -> PersistedSessionState {
    let mut state = PersistedSessionState::default();
    state.config_selections.insert(
        crate::shared_kernel::ConfigKey::new("collaboration_mode"),
        crate::shared_kernel::ConfigValue::new(value),
    );
    state
}

#[tokio::test]
async fn collaboration_restore_uses_generic_command_before_effort_without_extra_turn() {
    let backend = ConfigBackend::new();
    let snapshot = persisted_selection("plan");
    restore_persisted_config(
        backend.as_ref(),
        "codex",
        Some("high".into()),
        Some(&snapshot),
        "conv-1",
    )
    .await;
    let commands = backend.commands.lock().unwrap();
    assert_eq!(commands.len(), 2, "restore both tokens, without Send/Init");
    assert!(
        matches!(&commands[0], Command::SetConfigOption { option_id, value } if option_id == "collaboration_mode" && value == "plan")
    );
    assert!(
        matches!(&commands[1], Command::SetConfigOption { option_id, value } if option_id == "effort" && value == "high")
    );
    assert_eq!(backend.capabilities().current_mode.as_deref(), Some("on-request"));
}

#[tokio::test]
async fn collaboration_restore_non_codex_and_no_selection_do_not_add_commands() {
    for label in ["claude", "antigravity"] {
        let backend = ConfigBackend::new();
        restore_persisted_config(
            backend.as_ref(),
            label,
            None,
            Some(&persisted_selection("plan")),
            "conv-1",
        )
        .await;
        assert!(backend.commands.lock().unwrap().is_empty());
    }
    let backend = ConfigBackend::new();
    restore_persisted_config(backend.as_ref(), "codex", None, None, "conv-1").await;
    assert!(backend.commands.lock().unwrap().is_empty());
}

#[tokio::test]
async fn collaboration_restore_waits_for_late_discovery() {
    let backend = ConfigBackend::new();
    backend.caps.lock().unwrap().available_collaboration_modes.clear();
    let snapshot = persisted_selection("default");
    let restore = restore_persisted_config(backend.as_ref(), "codex", None, Some(&snapshot), "conv-1");
    tokio::pin!(restore);
    assert!(
        futures_util::poll!(&mut restore).is_pending(),
        "do not drop a persisted token before discovery"
    );
    backend.caps.lock().unwrap().available_collaboration_modes = vec![mode("plan"), mode("default")];
    restore.await;
    assert_eq!(
        backend.capabilities().current_collaboration_mode.as_deref(),
        Some("default")
    );
}

#[tokio::test]
async fn collaboration_ack_without_selection_does_not_fabricate_observed_or_persist() {
    let (repo, _db) = seeded_repo().await;
    let mut backend = ConfigBackend::new();
    Arc::get_mut(&mut backend).unwrap().do_not_observe = true;
    let task = task(backend, Some(repo.clone()));
    let response = task.set_config_option("collaboration_mode", "plan").await.unwrap();
    assert!(matches!(
        response.confirmation,
        aionui_api_types::ConfigOptionConfirmation::CommandAck
    ));
    assert_four(&response.config_options.unwrap(), None);
    assert!(
        repo.load_runtime_state_for_user("user-1", "conv-1")
            .await
            .unwrap()
            .unwrap_or_default()
            .config_selections_json
            .is_none()
    );
}

#[tokio::test]
async fn collaboration_restore_retries_reconcile_pending_without_sending_a_turn() {
    let backend = ConfigBackend::new();
    backend.pending_rejections.store(1, Ordering::SeqCst);
    restore_persisted_config(
        backend.as_ref(),
        "codex",
        None,
        Some(&persisted_selection("plan")),
        "conv-1",
    )
    .await;
    assert_eq!(
        backend.capabilities().current_collaboration_mode.as_deref(),
        Some("plan")
    );
    let commands = backend.commands.lock().unwrap();
    assert_eq!(commands.len(), 2);
    assert!(commands.iter().all(
        |command| matches!(command, Command::SetConfigOption { option_id, .. } if option_id == "collaboration_mode")
    ));
}

#[tokio::test]
async fn collaboration_restore_unavailable_is_bounded_and_keeps_effort_restore() {
    let backend = ConfigBackend::new();
    backend.caps.lock().unwrap().available_collaboration_modes.clear();
    tokio::time::timeout(
        std::time::Duration::from_secs(6),
        restore_persisted_config(
            backend.as_ref(),
            "codex",
            Some("high".into()),
            Some(&persisted_selection("plan")),
            "conv-1",
        ),
    )
    .await
    .expect("restore must remain bounded");
    assert_eq!(backend.capabilities().current_collaboration_mode, None);
    let commands = backend.commands.lock().unwrap();
    assert_eq!(commands.len(), 1);
    assert!(
        matches!(&commands[0], Command::SetConfigOption { option_id, value } if option_id == "effort" && value == "high")
    );
}

#[tokio::test]
async fn collaboration_restore_ignores_invalid_persisted_token() {
    let backend = ConfigBackend::new();
    restore_persisted_config(
        backend.as_ref(),
        "codex",
        None,
        Some(&persisted_selection("unknown")),
        "conv-1",
    )
    .await;
    assert!(backend.commands.lock().unwrap().is_empty());
}

#[tokio::test]
async fn collaboration_event_before_rest_keeps_sibling_axes() {
    let backend = ConfigBackend::new();
    let task = task(backend.clone(), None);
    let mut rx = task.runtime.tx.subscribe();
    backend.emit(SessionEvent::CollaborationModeUpdated {
        modes: vec![mode("plan"), mode("default")],
        selected_mode: Some("default".into()),
    });
    assert_four(&pushed(&mut rx).await, Some("default"));
}

async fn seeded_repo() -> (Arc<dyn IAcpSessionRepository>, aionui_db::Database) {
    let db = aionui_db::init_database_memory().await.unwrap();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at) VALUES ('user-1','user-1','hash',0,0)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query("INSERT INTO conversations (id,user_id,name,type,status,created_at,updated_at,extra) VALUES ('conv-1','user-1','conv-1','acp','pending',0,0,'{}')").execute(db.pool()).await.unwrap();
    let repo: Arc<dyn IAcpSessionRepository> = Arc::new(aionui_db::SqliteAcpSessionRepository::new(db.pool().clone()));
    repo.create(&aionui_db::CreateAcpSessionParams {
        user_id: "user-1",
        conversation_id: "conv-1",
        agent_source: "builtin",
        agent_id: "codex",
    })
    .await
    .unwrap();
    (repo, db)
}

struct ControlledRepo {
    inner: Arc<dyn IAcpSessionRepository>,
    loads: std::sync::atomic::AtomicUsize,
    gate_first_load: bool,
    first_read: tokio::sync::Notify,
    release: tokio::sync::Notify,
    fail_load: bool,
    fail_save: bool,
    missing_save: bool,
}

impl ControlledRepo {
    fn new(inner: Arc<dyn IAcpSessionRepository>) -> Self {
        Self {
            inner,
            loads: std::sync::atomic::AtomicUsize::new(0),
            gate_first_load: false,
            first_read: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            fail_load: false,
            fail_save: false,
            missing_save: false,
        }
    }
}

#[async_trait::async_trait]
impl IAcpSessionRepository for ControlledRepo {
    async fn get_for_user(
        &self,
        u: &str,
        c: &str,
    ) -> Result<Option<aionui_db::models::AcpSessionRow>, aionui_db::DbError> {
        self.inner.get_for_user(u, c).await
    }
    async fn create(
        &self,
        p: &aionui_db::CreateAcpSessionParams<'_>,
    ) -> Result<aionui_db::models::AcpSessionRow, aionui_db::DbError> {
        self.inner.create(p).await
    }
    async fn update_session_id_for_user(&self, u: &str, c: &str, s: &str) -> Result<bool, aionui_db::DbError> {
        self.inner.update_session_id_for_user(u, c, s).await
    }
    async fn clear_session_id_for_user(&self, u: &str, c: &str) -> Result<bool, aionui_db::DbError> {
        self.inner.clear_session_id_for_user(u, c).await
    }
    async fn delete_for_user(&self, u: &str, c: &str) -> Result<bool, aionui_db::DbError> {
        self.inner.delete_for_user(u, c).await
    }
    async fn load_runtime_state_for_user(
        &self,
        u: &str,
        c: &str,
    ) -> Result<Option<aionui_db::PersistedSessionState>, aionui_db::DbError> {
        let index = self.loads.fetch_add(1, Ordering::SeqCst);
        if self.fail_load {
            return Err(aionui_db::DbError::Init("test load failure".into()));
        }
        let state = self.inner.load_runtime_state_for_user(u, c).await?;
        if self.gate_first_load && index == 0 {
            self.first_read.notify_one();
            self.release.notified().await;
        }
        Ok(state)
    }
    async fn save_runtime_state_for_user(
        &self,
        u: &str,
        c: &str,
        p: &SaveRuntimeStateParams<'_>,
    ) -> Result<bool, aionui_db::DbError> {
        if self.fail_save {
            return Err(aionui_db::DbError::Init("test save failure".into()));
        }
        if self.missing_save {
            return Ok(false);
        }
        self.inner.save_runtime_state_for_user(u, c, p).await
    }
}

#[tokio::test]
async fn collaboration_concurrent_effort_merge_serializes_read_modify_write() {
    let (inner, _db) = seeded_repo().await;
    let mut repo = ControlledRepo::new(inner.clone());
    repo.gate_first_load = true;
    let repo = Arc::new(repo);
    let task = task(ConfigBackend::new(), Some(repo.clone()));
    let collaboration = task.set_config_option("collaboration_mode", "plan");
    tokio::pin!(collaboration);
    tokio::select! {
        _ = &mut collaboration => panic!("first selection must pause after reading"),
        _ = repo.first_read.notified() => {}
    }
    let effort = task.set_config_option("reasoning_effort", "high");
    tokio::pin!(effort);
    assert!(futures_util::poll!(&mut effort).is_pending());
    assert_eq!(
        repo.loads.load(Ordering::SeqCst),
        1,
        "a second selection must not read a stale map before the first write"
    );
    repo.release.notify_one();
    let (collaboration, effort) = tokio::join!(collaboration, effort);
    collaboration.unwrap();
    effort.unwrap();
    let state = inner
        .load_runtime_state_for_user("user-1", "conv-1")
        .await
        .unwrap()
        .unwrap();
    let selections: serde_json::Value = serde_json::from_str(state.config_selections_json.as_deref().unwrap()).unwrap();
    assert_eq!(
        selections,
        serde_json::json!({"collaboration_mode":"plan", "effort":"high"})
    );
}

#[tokio::test]
async fn collaboration_persistence_failure_does_not_undo_accepted_selection() {
    for failure in ["load", "save", "missing"] {
        let (inner, _db) = seeded_repo().await;
        let mut repo = ControlledRepo::new(inner.clone());
        repo.fail_load = failure == "load";
        repo.fail_save = failure == "save";
        repo.missing_save = failure == "missing";
        let task = task(ConfigBackend::new(), Some(Arc::new(repo)));
        let response = task.set_config_option("collaboration_mode", "plan").await.unwrap();
        assert!(matches!(
            response.confirmation,
            aionui_api_types::ConfigOptionConfirmation::Observed
        ));
        assert_four(&response.config_options.unwrap(), Some("plan"));
        let state = inner
            .load_runtime_state_for_user("user-1", "conv-1")
            .await
            .unwrap()
            .unwrap_or_default();
        assert!(state.config_selections_json.is_none());
    }
}

#[tokio::test]
async fn collaboration_persistence_merges_effort_and_other_keys() {
    let (repo, _db) = seeded_repo().await;
    repo.save_runtime_state_for_user(
        "user-1",
        "conv-1",
        &SaveRuntimeStateParams {
            config_selections_json: Some(Some(r#"{"effort":"high","unrelated":"keep"}"#)),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let task = task(ConfigBackend::new(), Some(repo.clone()));
    task.set_config_option("collaboration_mode", "plan").await.unwrap();
    let state = repo
        .load_runtime_state_for_user("user-1", "conv-1")
        .await
        .unwrap()
        .unwrap();
    let selections: serde_json::Value = serde_json::from_str(state.config_selections_json.as_deref().unwrap()).unwrap();
    assert_eq!(
        selections,
        serde_json::json!({"effort":"high", "unrelated":"keep", "collaboration_mode":"plan"})
    );
}
