//! 按需同步飞书发信人的通讯录姓名；缓存只用于显示，不参与身份授权。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aionui_db::IFeishuLoginRepository;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use super::{FeishuLogin, ResolvedConfig};

const LOOKUP_BUDGET: Duration = Duration::from_secs(3);
const NAME_TTL: Duration = Duration::from_secs(3600);
const MISSING_TTL: Duration = Duration::from_secs(300);
const FAILURE_TTL: Duration = Duration::from_secs(30);
const MAX_CONTACTS: usize = 512;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

#[async_trait::async_trait]
pub trait FeishuSenderNameResolver: Send + Sync {
    async fn resolve(&self, app_id: &str, tenant_key: &str, open_id: &str) -> Option<String>;
}

struct CachedContact {
    name: Option<String>,
    expires: Instant,
}

type ContactSlot = Arc<Mutex<Option<CachedContact>>>;

#[derive(Default)]
struct DirectoryCache {
    scope: [u8; 32],
    token: Option<(String, Instant)>,
    contacts: HashMap<String, ContactSlot>,
}

/// 使用现有服务器端加密配置，不向浏览器或调用方暴露应用凭据。
pub struct FeishuContacts {
    login: FeishuLogin,
    cache: Mutex<DirectoryCache>,
    token_refresh: Mutex<()>,
}

impl FeishuContacts {
    pub fn new(repo: Arc<dyn IFeishuLoginRepository>, encryption_key: [u8; 32]) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(2))
            .connect_timeout(Duration::from_secs(1))
            .build()?;
        Ok(Self {
            login: FeishuLogin::new(repo, encryption_key, http),
            cache: Mutex::new(DirectoryCache::default()),
            token_refresh: Mutex::new(()),
        })
    }

    async fn lookup(&self, app_id: &str, tenant_key: &str, open_id: &str) -> Option<String> {
        let cfg = self.login.resolved().await.ok()?;
        if open_id.is_empty() || cfg.app_id != app_id || cfg.tenant_key.as_deref() != Some(tenant_key) {
            return None;
        }
        let base = reqwest::Url::parse(&cfg.api_base).ok()?;
        let loopback = base.host_str().is_some_and(|host| {
            host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
        });
        if base.scheme() != "https" && !(base.scheme() == "http" && loopback) {
            return None;
        }
        let mut digest = Sha256::new();
        for value in [app_id, tenant_key, cfg.api_base.as_str(), cfg.app_secret.as_str()] {
            digest.update(value.as_bytes());
            digest.update([0]);
        }
        let scope: [u8; 32] = digest.finalize().into();
        let slot = {
            let mut cache = self.cache.lock().await;
            if cache.scope != scope {
                *cache = DirectoryCache {
                    scope,
                    ..Default::default()
                };
            }
            if let Some(slot) = cache.contacts.get(open_id) {
                slot.clone()
            } else {
                if cache.contacts.len() >= MAX_CONTACTS {
                    // 只淘汰没有在途调用引用的条目，不拆开同一发信人的同步。
                    let idle = cache
                        .contacts
                        .iter()
                        .find(|(_, slot)| Arc::strong_count(slot) == 1)
                        .map(|(key, _)| key.clone());
                    cache.contacts.remove(&idle?);
                }
                let slot = Arc::new(Mutex::new(None));
                cache.contacts.insert(open_id.to_owned(), slot.clone());
                slot
            }
        };
        // 网络等待只占用这个发信人的锁，其他姓名缓存不受影响。
        let mut contact = slot.lock().await;
        if self.cache.lock().await.scope != scope {
            return None;
        }
        if let Some(cached) = contact.as_ref().filter(|cached| cached.expires > Instant::now()) {
            return cached.name.clone();
        }
        // 即使调用被超时取消，也保留短期失败缓存，避免反复同步。
        *contact = Some(CachedContact {
            name: None,
            expires: Instant::now() + FAILURE_TTL,
        });
        tracing::info!(
            code = "FEISHU_CONTACT_SYNC_STARTED",
            "feishu: syncing missing sender display name"
        );
        match self.sync_contact(&cfg, open_id, scope).await {
            Ok(name) => {
                let ttl = if name.is_some() { NAME_TTL } else { MISSING_TTL };
                *contact = Some(CachedContact {
                    name: name.clone(),
                    expires: Instant::now() + ttl,
                });
                tracing::info!(
                    code = "FEISHU_CONTACT_SYNC_FINISHED",
                    name_found = name.is_some(),
                    "feishu: sender display name sync finished"
                );
                name
            }
            Err(code) => {
                tracing::warn!(
                    code,
                    "feishu: sender display name unavailable; external-user label used"
                );
                None
            }
        }
    }

    async fn tenant_token(&self, cfg: &ResolvedConfig, scope: [u8; 32]) -> Result<String, &'static str> {
        {
            let cache = self.cache.lock().await;
            if cache.scope != scope {
                return Err("FEISHU_CONTACT_CONFIG_CHANGED");
            }
            if let Some((token, _)) = cache.token.as_ref().filter(|(_, expires)| *expires > Instant::now()) {
                return Ok(token.clone());
            }
        }
        let _refresh = self.token_refresh.lock().await;
        {
            let cache = self.cache.lock().await;
            if cache.scope != scope {
                return Err("FEISHU_CONTACT_CONFIG_CHANGED");
            }
            if let Some((token, _)) = cache.token.as_ref().filter(|(_, expires)| *expires > Instant::now()) {
                return Ok(token.clone());
            }
        }
        #[derive(Deserialize)]
        struct Token {
            code: i64,
            tenant_access_token: Option<String>,
            expire: Option<u64>,
        }
        let response = self
            .login
            .http
            .post(format!(
                "{}/open-apis/auth/v3/tenant_access_token/internal",
                cfg.api_base
            ))
            .json(&serde_json::json!({"app_id":cfg.app_id,"app_secret":cfg.app_secret}))
            .send()
            .await
            .map_err(|_| "FEISHU_CONTACT_TOKEN_UNAVAILABLE")?;
        let reply: Token = read_json(response).await?;
        let token = reply
            .tenant_access_token
            .filter(|token| !token.is_empty() && reply.code == 0)
            .ok_or("FEISHU_CONTACT_TOKEN_REJECTED")?;
        let lifetime = reply.expire.unwrap_or(60).clamp(60, 86400) - 30;
        let mut cache = self.cache.lock().await;
        if cache.scope != scope {
            return Err("FEISHU_CONTACT_CONFIG_CHANGED");
        }
        cache.token = Some((token.clone(), Instant::now() + Duration::from_secs(lifetime)));
        Ok(token)
    }

    async fn sync_contact(
        &self,
        cfg: &ResolvedConfig,
        open_id: &str,
        scope: [u8; 32],
    ) -> Result<Option<String>, &'static str> {
        let token = self.tenant_token(cfg, scope).await?;
        let mut url = reqwest::Url::parse(&format!("{}/open-apis/contact/v3/users/", cfg.api_base))
            .map_err(|_| "FEISHU_CONTACT_URL_INVALID")?;
        url.path_segments_mut()
            .map_err(|_| "FEISHU_CONTACT_URL_INVALID")?
            .pop_if_empty()
            .push(open_id);
        url.query_pairs_mut().append_pair("user_id_type", "open_id");
        let response = self
            .login
            .http
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|_| "FEISHU_CONTACT_SYNC_UNAVAILABLE")?;
        #[derive(Deserialize)]
        struct Reply {
            code: i64,
            data: Option<Data>,
        }
        #[derive(Deserialize)]
        struct Data {
            user: Option<Contact>,
        }
        #[derive(Deserialize)]
        struct Contact {
            open_id: String,
            #[serde(default)]
            name: String,
            #[serde(default)]
            en_name: String,
        }
        let reply: Reply = read_json(response).await?;
        if reply.code != 0 {
            if reply.code == 99991663 {
                let mut cache = self.cache.lock().await;
                if cache.scope == scope {
                    cache.token = None;
                }
            }
            tracing::warn!(
                code = "FEISHU_CONTACT_SYNC_REJECTED",
                platform_code = reply.code,
                "feishu: contact API rejected sender name synchronization"
            );
            return Err("FEISHU_CONTACT_SYNC_REJECTED");
        }
        let Some(contact) = reply.data.and_then(|data| data.user) else {
            return Ok(None);
        };
        if contact.open_id != open_id {
            return Err("FEISHU_CONTACT_IDENTITY_MISMATCH");
        }
        Ok([contact.name, contact.en_name].into_iter().find_map(|name| {
            let normalized = name.split_whitespace().collect::<Vec<_>>().join(" ");
            let normalized: String = normalized.chars().filter(|ch| !ch.is_control()).take(128).collect();
            (!normalized.is_empty() && normalized != open_id).then_some(normalized)
        }))
    }
}

#[async_trait::async_trait]
impl FeishuSenderNameResolver for FeishuContacts {
    async fn resolve(&self, app_id: &str, tenant_key: &str, open_id: &str) -> Option<String> {
        match tokio::time::timeout(LOOKUP_BUDGET, self.lookup(app_id, tenant_key, open_id)).await {
            Ok(name) => name,
            Err(_) => {
                tracing::warn!(
                    code = "FEISHU_CONTACT_SYNC_TIMEOUT",
                    "feishu: bounded sender name lookup timed out"
                );
                None
            }
        }
    }
}

async fn read_json<T: DeserializeOwned>(mut response: reqwest::Response) -> Result<T, &'static str> {
    if !response.status().is_success() {
        return Err("FEISHU_CONTACT_HTTP_REJECTED");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "FEISHU_CONTACT_RESPONSE_UNREADABLE")?
    {
        if chunk.len() > MAX_RESPONSE_BYTES.saturating_sub(bytes.len()) {
            return Err("FEISHU_CONTACT_RESPONSE_TOO_LARGE");
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| "FEISHU_CONTACT_RESPONSE_INVALID")
}

#[cfg(test)]
#[path = "contacts_tests.rs"]
mod tests;
