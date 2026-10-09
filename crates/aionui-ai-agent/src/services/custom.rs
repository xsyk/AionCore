//! Custom Agent business logic.
//!
//! Extends `AgentService` with CRUD for `agent_source = 'custom'` rows
//! in the `agent_metadata` catalog. Mirrors the frontend PRD
//! F-CAGENT-04 / -05 / -12 / -13 / -14 (create, edit, save, delete,
//! toggle enable).
//!
//! Since 1.0.1 custom agents are server-wide, like the builtin catalog rows:
//! they are stored without an owner (`user_id` NULL) and every user sees and
//! runs them. The user id passed in here only names the acting user (for the
//! visibility lookups and for whom the probe reports progress); it is never an
//! owner. Who may create, edit or delete them is decided by the routes (the
//! administrator alone), not here.
//!
//! Test-on-save: create / update run `try_connect_custom_agent`
//! before hitting the DB. Failures become `AgentError::BadRequest` with
//! a prefixed marker (`cli_not_found:` / `acp_init_failed:`) that the
//! frontend maps back to the same three Alert states it shows for the
//! manual "Test connection" button.

use std::collections::HashMap;

use crate::error::AgentError;
use aionui_api_types::{
    AgentMetadata, CustomAgentUpsertRequest, TryConnectCustomAgentRequest, TryConnectCustomAgentResponse,
};
use aionui_common::generate_short_id;
use aionui_db::UpsertAgentMetadataParams;
use tracing::warn;

use super::AgentService;
use crate::protocol::custom_agent_probe::try_connect_custom_agent as probe;
use crate::runtime_status::custom_agent_runtime_reporter;

const CUSTOM_SORT_ORDER_DEFAULT: i64 = 1500;

/// How many generated ids `create_custom_agent` tries before giving up.
const CUSTOM_AGENT_ID_ATTEMPTS: usize = 5;

impl AgentService {
    /// Public accessor for the probe — powers both
    /// `POST /api/agents/custom/try-connect` and the test-on-save path
    /// below.
    pub async fn try_connect_custom_agent(
        &self,
        user_id: &str,
        req: TryConnectCustomAgentRequest,
    ) -> Result<TryConnectCustomAgentResponse, AgentError> {
        if req.command.trim().is_empty() {
            return Err(AgentError::bad_request("command must not be empty"));
        }
        let reporter = req.runtime_scope_id.as_ref().map(|scope_id| {
            custom_agent_runtime_reporter(self.broadcaster().clone(), user_id.to_owned(), scope_id.clone())
        });
        Ok(probe(&req.command, &req.acp_args, &req.env, reporter.as_deref()).await)
    }

    pub async fn create_custom_agent(
        &self,
        user_id: &str,
        req: CustomAgentUpsertRequest,
    ) -> Result<AgentMetadata, AgentError> {
        validate_upsert(&req)?;
        probe_or_reject(&req).await?;

        let id = self.unused_agent_id(generate_short_id).await?;
        self.upsert_custom_row(user_id, &id, &req, /* keep_enabled = */ true)
            .await
    }

    /// An id that no catalog row uses yet, drawn from `next_id` (at most
    /// [`CUSTOM_AGENT_ID_ATTEMPTS`] times).
    ///
    /// Custom agents are saved with the global upsert, whose conflict clause
    /// matches every ownerless row. The generated ids are only 8 characters, so
    /// reusing a builtin agent's id (or another custom agent's) would silently
    /// overwrite that row; look first instead.
    async fn unused_agent_id(&self, mut next_id: impl FnMut() -> String) -> Result<String, AgentError> {
        for _ in 0..CUSTOM_AGENT_ID_ATTEMPTS {
            let id = next_id();
            let taken = self
                .registry()
                .repo_handle()
                .get(&id)
                .await
                .map_err(|e| AgentError::internal(format!("repo.get: {e}")))?
                .is_some();
            if !taken {
                return Ok(id);
            }
            warn!(agent_id = %id, "generated custom agent id is already in use; generating another");
        }
        Err(AgentError::internal(format!(
            "could not generate an unused custom agent id in {CUSTOM_AGENT_ID_ATTEMPTS} attempts"
        )))
    }

    pub async fn update_custom_agent(
        &self,
        user_id: &str,
        id: &str,
        req: CustomAgentUpsertRequest,
    ) -> Result<AgentMetadata, AgentError> {
        validate_upsert(&req)?;
        let existing = self
            .registry()
            .repo_handle()
            .get_for_user(user_id, id)
            .await
            .map_err(|e| AgentError::internal(format!("repo.get_for_user: {e}")))?
            .ok_or_else(|| AgentError::not_found(format!("Agent '{id}' not found")))?;
        if existing.agent_source != "custom" {
            return Err(AgentError::forbidden(
                "Only custom agents can be edited via this endpoint",
            ));
        }
        probe_or_reject(&req).await?;

        let keep_enabled = existing.enabled;
        self.upsert_custom_row(user_id, id, &req, keep_enabled).await
    }

    pub async fn delete_custom_agent(&self, user_id: &str, id: &str) -> Result<(), AgentError> {
        let existing = self
            .registry()
            .repo_handle()
            .get_for_user(user_id, id)
            .await
            .map_err(|e| AgentError::internal(format!("repo.get_for_user: {e}")))?
            .ok_or_else(|| AgentError::not_found(format!("Agent '{id}' not found")))?;
        if existing.agent_source != "custom" {
            return Err(AgentError::forbidden(
                "Only custom agents can be deleted via this endpoint",
            ));
        }
        let removed = self
            .registry()
            .repo_handle()
            .delete_for_user(user_id, id)
            .await
            .map_err(|e| AgentError::internal(format!("repo.delete_for_user: {e}")))?;
        if !removed {
            return Err(AgentError::not_found(format!("Agent '{id}' not found")));
        }
        // Agents are machine-level, so the machine cache must be refreshed
        // regardless of which user triggered the change. A custom agent has no
        // owner, so the cache holds it; reload_one drops the removed row.
        if let Err(err) = self.registry().reload_one(id).await {
            warn!(agent_id = %id, error = %err, "registry reload failed after delete_custom_agent");
        }
        Ok(())
    }

    pub async fn set_agent_enabled(&self, user_id: &str, id: &str, enabled: bool) -> Result<AgentMetadata, AgentError> {
        let updated = self
            .registry()
            .repo_handle()
            .set_enabled_for_user(user_id, id, enabled)
            .await
            .map_err(|e| AgentError::internal(format!("repo.set_enabled_for_user: {e}")))?;
        if !updated {
            return Err(AgentError::not_found(format!("Agent '{id}' not found")));
        }
        // enabled is machine-level and gates the registry's runtime start, so
        // any user's toggle must refresh the machine cache — not just the
        // default user's. (Previously gated on SYSTEM_DEFAULT_USER_ID, which
        // left a builtin toggled by another user stale in the cache.)
        if let Err(err) = self.registry().reload_one(id).await {
            warn!(agent_id = %id, error = %err, "registry reload failed after set_agent_enabled");
        }
        self.registry()
            .get_for_user(user_id, id)
            .await?
            .ok_or_else(|| AgentError::internal(format!("Agent '{id}' not visible after enable toggle")))
    }

    async fn upsert_custom_row(
        &self,
        user_id: &str,
        id: &str,
        req: &CustomAgentUpsertRequest,
        enabled: bool,
    ) -> Result<AgentMetadata, AgentError> {
        let advanced = req.advanced.clone().unwrap_or_default();

        let args_json =
            serde_json::to_string(&req.args).map_err(|e| AgentError::internal(format!("encode args: {e}")))?;
        let env_json = serde_json::to_string(&req.env).map_err(|e| AgentError::internal(format!("encode env: {e}")))?;
        let native_skills_dirs_json = advanced
            .native_skills_dirs
            .as_ref()
            .map(|v| {
                serde_json::to_string(v).map_err(|e| AgentError::internal(format!("encode native_skills_dirs: {e}")))
            })
            .transpose()?;
        let behavior_policy_json = advanced
            .behavior_policy
            .as_ref()
            .map(|v| serde_json::to_string(v).map_err(|e| AgentError::internal(format!("encode behavior_policy: {e}"))))
            .transpose()?;

        let source_info = serde_json::json!({
            "binary_name": first_token(&req.command),
        });
        let source_info_json = source_info.to_string();

        let params = UpsertAgentMetadataParams {
            id,
            icon: req.icon.as_deref(),
            name: req.name.trim(),
            name_i18n: None,
            description: advanced.description.as_deref(),
            description_i18n: None,
            backend: None,
            agent_type: "acp",
            agent_source: "custom",
            agent_source_info: Some(&source_info_json),
            enabled,
            command: Some(req.command.trim()),
            args: Some(&args_json),
            env: Some(&env_json),
            native_skills_dirs: native_skills_dirs_json.as_deref(),
            skill_delivery: None,
            behavior_policy: behavior_policy_json.as_deref(),
            yolo_id: advanced.yolo_id.as_deref(),
            agent_capabilities: None,
            auth_methods: None,
            config_options: None,
            available_modes: None,
            available_models: None,
            available_commands: None,
            sort_order: CUSTOM_SORT_ORDER_DEFAULT,
        };

        // Written without an owner so every user sees it, whoever saved it
        // (the administrator may be acting as another user).
        self.registry()
            .repo_handle()
            .upsert_global(&params)
            .await
            .map_err(|e| AgentError::internal(format!("repo.upsert_global: {e}")))?;

        // Machine-level cache refresh: the row has no owner, so the cache holds it.
        self.registry()
            .reload_one(id)
            .await
            .map_err(|e| AgentError::internal(format!("registry reload: {e}")))?;

        self.registry()
            .get_for_user(user_id, id)
            .await?
            .ok_or_else(|| AgentError::internal(format!("Agent '{id}' not visible after upsert")))
    }
}

fn validate_upsert(req: &CustomAgentUpsertRequest) -> Result<(), AgentError> {
    if req.name.trim().is_empty() {
        return Err(AgentError::bad_request("name must not be empty"));
    }
    if req.command.trim().is_empty() {
        return Err(AgentError::bad_request("command must not be empty"));
    }
    Ok(())
}

async fn probe_or_reject(req: &CustomAgentUpsertRequest) -> Result<(), AgentError> {
    // Test-only bypass — real probe spawns a child process and relies
    // on a working ACP CLI on PATH, which is not present in CI.
    // Gated behind cfg(test) / the `test-support` feature so production
    // builds cannot be tricked into skipping the probe via env var.
    #[cfg(any(test, feature = "test-support"))]
    if std::env::var("AIONUI_BYPASS_PROBE").is_ok() {
        tracing::warn!("AIONUI_BYPASS_PROBE set — skipping custom agent probe. Test-only.");
        return Ok(());
    }

    let env_map: HashMap<String, String> = req.env.iter().map(|e| (e.name.clone(), e.value.clone())).collect();
    match probe(&req.command, &req.args, &env_map, None).await {
        TryConnectCustomAgentResponse::Success => Ok(()),
        // Reachable but not authorized is a valid agent the user simply hasn't
        // logged into yet — accept the save so it lands in the list (offline,
        // needs-login), where a later "test connection" confirms recovery.
        TryConnectCustomAgentResponse::FailAuth { error } => {
            tracing::info!(%error, "custom agent reachable but requires auth; accepting save");
            Ok(())
        }
        TryConnectCustomAgentResponse::FailCli { error } => {
            Err(AgentError::bad_request(format!("cli_not_found: {error}")))
        }
        TryConnectCustomAgentResponse::FailAcp { error } => {
            Err(AgentError::bad_request(format!("acp_init_failed: {error}")))
        }
    }
}

fn first_token(s: &str) -> &str {
    s.split_whitespace().next().unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::Arc;

    use aionui_db::{
        IAgentMetadataRepository, IProviderRepository, SqliteAgentMetadataRepository, SqliteProviderRepository,
        UpsertAgentMetadataParams, init_database_memory,
    };
    use aionui_realtime::EventBroadcaster;

    use super::*;
    use crate::registry::AgentRegistry;

    struct NoopBroadcaster;

    impl EventBroadcaster for NoopBroadcaster {
        fn broadcast(&self, _msg: aionui_api_types::WebSocketMessage<serde_json::Value>) {}
    }

    /// A service over an in-memory database that already holds the builtin agent `taken`.
    async fn service_with_builtin(taken: &str) -> Arc<AgentService> {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));
        repo.upsert_global(&UpsertAgentMetadataParams {
            id: taken,
            icon: None,
            name: "Builtin",
            name_i18n: None,
            description: None,
            description_i18n: None,
            backend: Some("claude"),
            agent_type: "acp",
            agent_source: "builtin",
            agent_source_info: None,
            enabled: true,
            command: None,
            args: Some("[]"),
            env: Some("[]"),
            native_skills_dirs: None,
            skill_delivery: None,
            behavior_policy: None,
            yolo_id: None,
            agent_capabilities: None,
            auth_methods: None,
            config_options: None,
            available_modes: None,
            available_models: None,
            available_commands: None,
            sort_order: 100,
        })
        .await
        .unwrap();
        let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        AgentService::new(
            AgentRegistry::new(repo),
            Arc::new(NoopBroadcaster),
            provider_repo,
            [0; 32],
            std::env::temp_dir(),
        )
    }

    #[tokio::test]
    async fn an_id_nobody_uses_is_taken_as_generated() {
        let service = service_with_builtin("1a2b3c4d").await;

        let id = service.unused_agent_id(|| "ffffffff".to_owned()).await.unwrap();

        assert_eq!(id, "ffffffff");
    }

    #[tokio::test]
    async fn an_id_a_builtin_agent_uses_is_never_handed_out() {
        // The global upsert would overwrite the builtin row with the new custom agent.
        let service = service_with_builtin("1a2b3c4d").await;
        let mut candidates = ["1a2b3c4d", "5e6f7a8b"].into_iter().map(str::to_owned);

        let id = service
            .unused_agent_id(|| candidates.next().expect("asked for more ids than expected"))
            .await
            .unwrap();

        assert_eq!(id, "5e6f7a8b");
    }

    #[tokio::test]
    async fn gives_up_with_an_internal_error_after_a_bounded_number_of_collisions() {
        let service = service_with_builtin("1a2b3c4d").await;
        let attempts = Cell::new(0);

        let err = service
            .unused_agent_id(|| {
                attempts.set(attempts.get() + 1);
                "1a2b3c4d".to_owned()
            })
            .await
            .unwrap_err();

        assert_eq!(attempts.get(), CUSTOM_AGENT_ID_ATTEMPTS);
        assert!(matches!(err, AgentError::Internal(_)), "{err:?}");
    }
}
