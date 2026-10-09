//! Optional external notification hook. Error payloads go through stdin, never a shell.
use std::{ffi::OsStr, process::Stdio, time::Duration};

use aionui_ai_agent::AgentSessionKind;
use aionui_api_types::AgentStreamErrorData;
use tokio::{io::AsyncWriteExt, sync::Semaphore};
use tracing::{info, warn};

static NOTIFICATION_SLOTS: Semaphore = Semaphore::const_new(4);

/// Notify only after recovery has finished. The script is explicitly configured
/// by the operator; no webhook or notification destination ships in the binary.
pub(crate) fn notify(
    conversation_id: &str,
    turn_id: &str,
    error: Option<&AgentStreamErrorData>,
    agent: &AgentSessionKind,
) {
    let Some(script) = std::env::var_os("AIONUI_ERROR_NOTIFY_SCRIPT") else {
        return;
    };
    let Ok(permit) = NOTIFICATION_SLOTS.try_acquire() else {
        warn!(%conversation_id, %turn_id, "conversation error notification skipped: concurrency limit");
        return;
    };
    // Do not export raw agent text: it can contain prompts, credentials or files.
    let text = error.map(|e| format!("{} {}", e.message, e.detail.as_deref().unwrap_or_default()));
    let situation = safe_situation(text.as_deref().unwrap_or_default());
    let (agent_name, agent_backend, agent_id) = agent_identity(agent);
    let payload = serde_json::json!({
        "agent_name": agent_name,
        "agent_backend": agent_backend,
        "agent_id": agent_id,
        "conversation_id": conversation_id,
        "turn_id": turn_id,
        "code": error.and_then(|e| e.code),
        "ownership": error.and_then(|e| e.ownership),
        "retryable": error.and_then(|e| e.retryable),
        "situation": situation,
    });
    let conversation_id = conversation_id.to_owned();
    let turn_id = turn_id.to_owned();
    tokio::spawn(async move {
        let _permit = permit;
        match deliver(&script, &payload, Duration::from_secs(15)).await {
            Ok(()) => {
                info!(%conversation_id, %turn_id, agent_name = ?payload["agent_name"], agent_backend = ?payload["agent_backend"], agent_id = ?payload["agent_id"], "conversation error notification delivered");
            }
            Err(error) => {
                warn!(%conversation_id, %turn_id, error_kind = ?error.kind(), "conversation error notification failed")
            }
        }
    });
}

fn agent_identity(agent: &AgentSessionKind) -> (&str, &str, Option<&str>) {
    let (config, fallback) = match agent {
        AgentSessionKind::Acp(context) => (&context.config, "ACP"),
        AgentSessionKind::Antigravity(context) => (&context.config, "Antigravity"),
        AgentSessionKind::Aionrs(_) => return ("Aion CLI", "aionrs", None),
    };
    let backend = config.backend.as_deref().filter(|s| !s.is_empty()).unwrap_or(fallback);
    let name = config
        .agent_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(backend);
    (
        name,
        backend,
        config.agent_id.as_deref().or(config.custom_agent_id.as_deref()),
    )
}

async fn deliver(script: &OsStr, payload: &serde_json::Value, deadline: Duration) -> std::io::Result<()> {
    tokio::time::timeout(deadline, async {
        let mut spawn_attempt = 0;
        let mut child = loop {
            let mut command = aionui_runtime::Builder::clean_cli(script);
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            match command.spawn() {
                Ok(child) => break child,
                // A concurrent fork can briefly retain a writer to a newly
                // installed executable. Retry this launch error only: once a
                // hook starts it must never be replayed (duplicate delivery).
                Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy && spawn_attempt < 3 => {
                    spawn_attempt += 1;
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(error) => return Err(error),
            }
        };
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(payload.to_string().as_bytes()).await?;
            stdin.shutdown().await?;
        }
        let status = child.wait().await?;
        if status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other("notification hook exited unsuccessfully"))
        }
    })
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "notification hook timed out"))?
}

fn safe_situation(text: &str) -> &'static str {
    let text = text.to_ascii_lowercase();
    if text.contains("oauth session expired") {
        "OAuth 登录会话过期，刷新失败"
    } else if text.contains("workspace routing discovery unauthorized") {
        "工作区路由发现鉴权失败（401）"
    } else if text.contains("blocked by our safety systems") {
        "请求被安全系统拦截"
    } else {
        "会话轮次最终失败；请根据错误码及会话 ID 查看本地诊断"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_identifies_custom_agent_without_exporting_configuration() {
        let agent = AgentSessionKind::Acp(Box::new(aionui_ai_agent::AcpSessionBuildContext {
            config: aionui_api_types::AcpBuildExtra {
                agent_name: Some("Custom Codex".into()),
                backend: Some("codex".into()),
                custom_agent_id: Some("custom-id".into()),
                preset_context: Some("private prompt".into()),
                ..Default::default()
            },
            team: None,
            belongs_to_team: false,
            session_id: None,
            session_snapshot: None,
        }));
        assert_eq!(agent_identity(&agent), ("Custom Codex", "codex", Some("custom-id")));
        let AgentSessionKind::Acp(mut context) = agent else {
            unreachable!()
        };
        context.config.agent_name = None;
        assert_eq!(
            agent_identity(&AgentSessionKind::Acp(context)),
            ("codex", "codex", Some("custom-id"))
        );
    }

    #[cfg(unix)]
    fn hook(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("notify hook.sh");
        std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        script
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hook_receives_json_via_stdin_without_shell_interpolation() {
        let dir = tempfile::tempdir().unwrap();
        let script = hook(dir.path(), "cat > \"$0.payload\"");
        let payload = serde_json::json!({"turn_id": "$(touch SHOULD_NOT_EXIST)", "situation": "安全系统拦截"});
        deliver(script.as_os_str(), &payload, Duration::from_secs(5))
            .await
            .unwrap();
        let received: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("notify hook.sh.payload")).unwrap()).unwrap();
        assert_eq!(received, payload);
        assert!(!dir.path().join("SHOULD_NOT_EXIST").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unsuccessful_hook_is_a_delivery_failure() {
        let dir = tempfile::tempdir().unwrap();
        let script = hook(dir.path(), "cat > /dev/null; exit 7");
        let error = deliver(script.as_os_str(), &serde_json::json!({}), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hanging_hook_is_bounded_by_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let script = hook(dir.path(), "cat > /dev/null; exec sleep 60");
        let error = deliver(script.as_os_str(), &serde_json::json!({}), Duration::from_millis(100))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn notification_never_exports_arbitrary_error_text() {
        assert_eq!(safe_situation("secret prompt token=abc"), safe_situation(""));
        assert_eq!(
            safe_situation("Failed to authenticate: OAuth session expired token=abc"),
            "OAuth 登录会话过期，刷新失败"
        );
        assert_eq!(
            safe_situation("workspace routing discovery unauthorized (401)"),
            "工作区路由发现鉴权失败（401）"
        );
        assert_eq!(
            safe_situation("This request was blocked by our safety systems. private text"),
            "请求被安全系统拦截"
        );
    }
}
