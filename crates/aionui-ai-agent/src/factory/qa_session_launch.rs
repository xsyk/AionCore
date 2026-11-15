//! Process-local, exact-session launch authority for an opt-in diagnostic build.
//! No request/config deserialization can construct a validated launch value.

use std::path::PathBuf;
use std::sync::Arc;

use aionui_common::EnvVar;
use aionui_db::IConversationRepository;

use crate::error::AgentError;

#[cfg(all(feature = "qa-session-launch", target_os = "linux"))]
mod enabled;
#[cfg(all(feature = "qa-session-launch", target_os = "linux"))]
mod files;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct QaLaunchError(&'static str);

/// Borrowed, unvalidated environment inputs with their factory sources kept distinct.
pub(crate) struct QaLaunchEnvironment<'a> {
    pub(crate) metadata_env: &'a [aionui_api_types::AgentEnvEntry],
    pub(crate) runtime_env: &'a [(String, String)],
}

/// Opaque startup authority. Production has no path/UID override constructor.
pub struct QaSessionLaunchPolicy {
    #[cfg(all(feature = "qa-session-launch", target_os = "linux"))]
    inner: enabled::Policy,
}

impl QaSessionLaunchPolicy {
    /// Called before application services are constructed. Default builds do no IO.
    pub fn from_startup() -> Result<Option<Arc<Self>>, QaLaunchError> {
        #[cfg(all(feature = "qa-session-launch", target_os = "linux"))]
        {
            enabled::Policy::from_startup()
                .map(|inner| Some(Arc::new(Self { inner })))
                .map_err(|_| QaLaunchError("QA_SCOPE_CONFIG_INVALID"))
        }
        #[cfg(all(feature = "qa-session-launch", not(target_os = "linux")))]
        {
            Err(QaLaunchError("QA_SCOPE_PLATFORM_UNSUPPORTED"))
        }
        #[cfg(not(feature = "qa-session-launch"))]
        {
            Ok(None)
        }
    }

    pub(crate) async fn resolve(
        &self,
        backend: &str,
        conversation_id: &str,
        user_id: &str,
        repo: Option<&dyn IConversationRepository>,
        env: QaLaunchEnvironment<'_>,
        is_fork: bool,
    ) -> Result<Option<ValidatedQaLaunch>, AgentError> {
        #[cfg(all(feature = "qa-session-launch", target_os = "linux"))]
        {
            self.inner
                .resolve(backend, conversation_id, user_id, repo, env, is_fork)
                .await
                .map_err(|error| {
                    tracing::warn!(reason_code = error.0, "QA launch rejected");
                    AgentError::forbidden(error.0)
                })
        }
        #[cfg(not(all(feature = "qa-session-launch", target_os = "linux")))]
        {
            let _ = (
                backend,
                conversation_id,
                user_id,
                repo,
                env.metadata_env,
                env.runtime_env,
                is_fork,
            );
            Ok(None)
        }
    }
}

/// Owned proof passed only from the trusted factory into session assembly.
/// Deliberately neither Deserialize nor Clone; all fields are private.
pub struct ValidatedQaLaunch {
    conversation_id: String,
    user_id: String,
    program: PathBuf,
    env: [EnvVar; 4],
    expires_at: u64,
}

fn reserved_key(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    key.starts_with("AION_QA_") || key.starts_with("AIONUI_QA_")
}

fn unix_now() -> Result<u64, QaLaunchError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| QaLaunchError("QA_CLOCK_INVALID"))
}

impl ValidatedQaLaunch {
    pub(crate) fn apply(
        self,
        backend: &str,
        conversation_id: &str,
        user_id: &str,
        config: &mut aionui_session::SessionConfig,
        catalog_writeback: &mut Option<(String, crate::registry::CatalogSender)>,
        prompt_dump_dir: &mut Option<PathBuf>,
    ) -> Result<(), AgentError> {
        if backend != "codex" || self.conversation_id != conversation_id || self.user_id != user_id {
            return Err(AgentError::forbidden("QA_LAUNCH_CONTEXT_MISMATCH"));
        }
        if self.expires_at <= unix_now().map_err(|error| AgentError::forbidden(error.0))? {
            return Err(AgentError::forbidden("QA_BINDING_EXPIRED"));
        }
        if config.spawn_env.iter().any(|entry| reserved_key(&entry.name)) {
            return Err(AgentError::forbidden("QA_RESERVED_ENV_CONFLICT"));
        }
        config.cli_program = Some(self.program);
        config.spawn_env.extend(self.env);
        *catalog_writeback = None;
        *prompt_dump_dir = None;
        Ok(())
    }
}

/// Run this exact ignored test binary as the deployment service identity before
/// switching services. It exercises the same fixed-path startup gate as Core.
#[cfg(all(test, feature = "qa-session-launch", target_os = "linux"))]
#[test]
#[ignore = "explicit deployment preflight against installed root-owned scope"]
fn qa_session_launch_installed_scope_preflight() {
    assert!(
        QaSessionLaunchPolicy::from_startup()
            .expect("QA_SCOPE_CONFIG_INVALID")
            .is_some()
    );
}

#[cfg(all(test, not(feature = "qa-session-launch")))]
mod default_tests {
    use super::*;

    #[test]
    fn qa_session_launch_default_build_is_disabled_without_scope_io() {
        assert!(QaSessionLaunchPolicy::from_startup().unwrap().is_none());
    }

    #[tokio::test]
    async fn qa_session_launch_default_policy_does_not_inspect_requests_or_repository() {
        let policy = QaSessionLaunchPolicy {};
        let env = [("AION_QA_UNTRUSTED".into(), "request-value".into())];
        assert!(
            policy
                .resolve(
                    "codex",
                    "../unsafe",
                    "",
                    None,
                    QaLaunchEnvironment {
                        metadata_env: &[],
                        runtime_env: &env
                    },
                    true
                )
                .await
                .unwrap()
                .is_none()
        );
    }
}
