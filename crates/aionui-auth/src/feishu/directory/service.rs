//! 通讯录同步任务及账号导入；不依赖HTTP路由类型。

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use aionui_api_types::FeishuDirectorySyncView;
use aionui_db::{IFeishuLoginRepository, IUserRepository};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use super::accounts::{ImportOutcome, import_member};
use super::client::DirectoryReader;
use crate::feishu::{FeishuLogin, ResolvedConfig};

const SYNC_BUDGET: Duration = Duration::from_secs(15 * 60);
const MAX_MEMBERS: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DirectoryError {
    #[error("FEISHU_DIRECTORY_PERMISSION_DENIED")]
    PermissionDenied,
    #[error("FEISHU_DIRECTORY_CONFIG_UNAVAILABLE")]
    ConfigUnavailable,
    #[error("FEISHU_DIRECTORY_CONFIG_CHANGED")]
    ConfigChanged,
    #[error("FEISHU_DIRECTORY_TIMEOUT")]
    Timeout,
    #[error("FEISHU_DIRECTORY_SYNC_FAILED")]
    Failed,
}

impl DirectoryError {
    pub fn code(self) -> &'static str {
        match self {
            Self::PermissionDenied => "FEISHU_DIRECTORY_PERMISSION_DENIED",
            Self::ConfigUnavailable => "FEISHU_DIRECTORY_CONFIG_UNAVAILABLE",
            Self::ConfigChanged => "FEISHU_DIRECTORY_CONFIG_CHANGED",
            Self::Timeout => "FEISHU_DIRECTORY_TIMEOUT",
            Self::Failed => "FEISHU_DIRECTORY_SYNC_FAILED",
        }
    }
}

pub struct FeishuDirectorySync {
    login: FeishuLogin,
    users: Arc<dyn IUserRepository>,
    http: reqwest::Client,
    latest: Mutex<Option<FeishuDirectorySyncView>>,
}

impl FeishuDirectorySync {
    pub fn new(
        repo: Arc<dyn IFeishuLoginRepository>,
        key: [u8; 32],
        users: Arc<dyn IUserRepository>,
        http: reqwest::Client,
    ) -> Self {
        Self {
            login: FeishuLogin::new(repo, key, http.clone()),
            users,
            http,
            latest: Mutex::new(None),
        }
    }

    pub async fn latest(&self) -> Option<FeishuDirectorySyncView> {
        self.latest.lock().await.clone()
    }

    pub async fn start(self: &Arc<Self>) -> Result<FeishuDirectorySyncView, DirectoryError> {
        let mut latest = self.latest.lock().await;
        if let Some(view) = latest.as_ref().filter(|view| view.status == "running") {
            return Ok(view.clone());
        }
        let cfg = self
            .login
            .resolved()
            .await
            .map_err(|_| DirectoryError::ConfigUnavailable)?;
        super::client::validate_base(&cfg.api_base)?;
        let view = FeishuDirectorySyncView {
            id: aionui_common::generate_prefixed_id("directory"),
            status: "running".into(),
            scanned: 0,
            created: 0,
            updated: 0,
            unchanged: 0,
            skipped: 0,
            failed: 0,
            partial: false,
            error_code: None,
            started_at: aionui_common::now_ms(),
            finished_at: None,
        };
        *latest = Some(view.clone());
        let service = self.clone();
        tokio::spawn(async move {
            tracing::info!(code = "FEISHU_DIRECTORY_SYNC_STARTED", "feishu: directory sync started");
            let result = tokio::time::timeout(SYNC_BUDGET, service.run(&cfg)).await;
            let failure = match result {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error),
                Err(_) => Some(DirectoryError::Timeout),
            };
            let mut latest = service.latest.lock().await;
            if let Some(view) = latest.as_mut() {
                if let Some(error) = failure {
                    record_failure(view, error);
                }
                view.status = if view.failed == 0 { "completed" } else { "failed" }.into();
                view.partial = view.failed > 0 && view.created + view.updated + view.unchanged > 0;
                view.finished_at = Some(aionui_common::now_ms());
                tracing::info!(
                    code = "FEISHU_DIRECTORY_SYNC_FINISHED",
                    scanned = view.scanned,
                    created = view.created,
                    updated = view.updated,
                    unchanged = view.unchanged,
                    skipped = view.skipped,
                    failed = view.failed,
                    partial = view.partial,
                    error_code = view.error_code.as_deref(),
                    "feishu: directory sync finished"
                );
            }
        });
        Ok(view)
    }

    async fn ensure_config(&self, expected: [u8; 32]) -> Result<(), DirectoryError> {
        let cfg = self.login.resolved().await.map_err(|_| DirectoryError::ConfigChanged)?;
        if scope(&cfg) != expected {
            return Err(DirectoryError::ConfigChanged);
        }
        Ok(())
    }

    async fn run(&self, cfg: &ResolvedConfig) -> Result<(), DirectoryError> {
        let expected = scope(cfg);
        let mut reader = DirectoryReader::new(self.http.clone(), cfg.clone());
        let mut seen = HashSet::new();
        while !reader.is_empty() {
            self.ensure_config(expected).await?;
            let page = reader.next().await;
            // 配置在网络请求期间变化时，不使用旧应用或旧策略继续写入。
            self.ensure_config(expected).await?;
            let members = match page {
                Ok(members) => members,
                Err(error) => {
                    let mut latest = self.latest.lock().await;
                    if let Some(view) = latest.as_mut() {
                        record_failure(view, error);
                    }
                    continue;
                }
            };
            for member in members {
                let key = member
                    .union_id
                    .as_deref()
                    .filter(|id| !id.trim().is_empty())
                    .map(|id| format!("union:{id}"))
                    .or_else(|| member.open_id.as_deref().map(|id| format!("open:{id}")));
                if let Some(key) = key {
                    if seen.contains(&key) {
                        continue;
                    }
                    if seen.len() >= MAX_MEMBERS {
                        return Err(DirectoryError::Failed);
                    }
                    seen.insert(key);
                }
                self.ensure_config(expected).await?;
                let outcome = import_member(self.users.as_ref(), &member, cfg.signup_policy).await;
                let mut latest = self.latest.lock().await;
                if let Some(view) = latest.as_mut() {
                    view.scanned += 1;
                    match outcome {
                        Ok(ImportOutcome::Created) => view.created += 1,
                        Ok(ImportOutcome::CreatedWithProfileFailure) => {
                            view.created += 1;
                            record_failure(view, DirectoryError::Failed);
                        }
                        Ok(ImportOutcome::Updated) => view.updated += 1,
                        Ok(ImportOutcome::Unchanged) => view.unchanged += 1,
                        Ok(ImportOutcome::Skipped) => view.skipped += 1,
                        Err(error) => record_failure(view, error),
                    }
                }
            }
        }
        Ok(())
    }
}

fn record_failure(view: &mut FeishuDirectorySyncView, error: DirectoryError) {
    view.failed += 1;
    view.error_code.get_or_insert_with(|| error.code().into());
}

fn scope(cfg: &ResolvedConfig) -> [u8; 32] {
    let mut hash = Sha256::new();
    for value in [
        &cfg.app_id,
        &cfg.app_secret,
        &cfg.api_base,
        cfg.tenant_key.as_deref().unwrap_or_default(),
        cfg.signup_policy.as_str(),
    ] {
        hash.update(value.as_bytes());
        hash.update([0]);
    }
    hash.finalize().into()
}
