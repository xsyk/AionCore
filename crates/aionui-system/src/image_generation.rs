//! The image generation model: one setting for the whole server.
//!
//! The administrator picks a provider and one of its models. While the setting
//! is on, every agent session gets the image generation MCP server: a Node
//! script that ships next to the backend and reads the provider it talks to
//! from environment variables.
//!
//! The choice lives in the `global_settings` table under
//! [`IMAGE_GENERATION_SETTING_KEY`]. It holds ids, never secrets: the API key is
//! read from the provider each time a session server is built, so changing the
//! provider's key or model list needs no second step here.
//!
//! Building the server runs on the start path of every session, so it must stay
//! quick: Node is looked up in the background by `NodeCache` (see the
//! `node_cache` module), and a session waits for it a few seconds at most before
//! it starts without the tool.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aionui_api_types::{
    ImageGenerationSettingsResponse, SessionMcpServer, SessionMcpTransport, UpdateImageGenerationSettingsRequest,
};
use aionui_common::decrypt_string;
use aionui_db::{IGlobalSettingRepository, IProviderRepository};
use aionui_runtime::ResolvedCommand;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::error::SystemError;
use crate::node_cache::{NodeCache, NodeLookup};

pub use aionui_api_types::IMAGE_GENERATION_MCP_NAME;

/// `global_settings` key holding the image generation setting.
pub const IMAGE_GENERATION_SETTING_KEY: &str = "tools.imageGeneration";

// The environment variables the image generation script reads its provider from.
const ENV_PROVIDER_ID: &str = "AIONUI_IMG_PROVIDER_ID";
const ENV_PLATFORM: &str = "AIONUI_IMG_PLATFORM";
const ENV_BASE_URL: &str = "AIONUI_IMG_BASE_URL";
const ENV_API_KEY: &str = "AIONUI_IMG_API_KEY";
const ENV_MODEL: &str = "AIONUI_IMG_MODEL";

/// What is stored under [`IMAGE_GENERATION_SETTING_KEY`]. Every field falls back
/// to "off" so a partial or older value still reads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct StoredImageGeneration {
    #[serde(default)]
    provider_id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    enabled: bool,
}

/// Business logic for the shared image generation setting and for the MCP
/// server sessions get from it.
///
/// Reading the setting is open to every user, changing it is the
/// administrator's alone; who may do what is decided by the caller.
#[derive(Clone)]
pub struct ImageGenerationService {
    settings: Arc<dyn IGlobalSettingRepository>,
    providers: Arc<dyn IProviderRepository>,
    encryption_key: [u8; 32],
    /// The image generation MCP script, when this server is told where it is.
    script: Option<PathBuf>,
    /// Where Node is found; shared by every clone of the service.
    node: NodeCache,
}

impl ImageGenerationService {
    /// `script` is where the image generation MCP script is expected; image
    /// generation counts as supported only while a file is actually there.
    ///
    /// Node is not looked for yet: call [`Self::warm_up`] once the service is
    /// built, so a setting that is already on has its runtime ready by the time
    /// the first session asks for it.
    pub fn new(
        settings: Arc<dyn IGlobalSettingRepository>,
        providers: Arc<dyn IProviderRepository>,
        encryption_key: [u8; 32],
        script: Option<PathBuf>,
    ) -> Self {
        Self {
            settings,
            providers,
            encryption_key,
            // The agent runs in a conversation workspace, so a relative path
            // would not find the script there.
            script: script.map(|path| std::path::absolute(&path).unwrap_or(path)),
            node: NodeCache::new(Arc::new(|| -> NodeLookup { Box::pin(locate_node()) })),
        }
    }

    /// Replace how Node is located. Tests use this so they do not depend on the
    /// runtime installed on the machine. What was found so far is forgotten.
    pub fn with_node_resolver<F, Fut>(mut self, resolve: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Option<ResolvedCommand>> + Send + 'static,
    {
        self.node = NodeCache::new(Arc::new(move || -> NodeLookup { Box::pin(resolve()) }));
        self
    }

    /// Start looking for Node in the background when image generation is already
    /// on, so the first session after a restart does not have to wait for it.
    /// Call once after construction; it returns as soon as the setting is read.
    pub async fn warm_up(&self) {
        let enabled = match self.read_stored().await {
            Ok(stored) => stored.enabled,
            Err(error) => {
                warn!(%error, "image generation: could not read the setting; Node is not looked up ahead of time");
                return;
            }
        };
        if enabled && self.supported().await {
            self.node.warm_up();
        }
    }

    /// Whether this server has the image generation MCP script installed.
    pub async fn supported(&self) -> bool {
        self.installed_script().await.is_some()
    }

    /// The current setting; an unset (or unreadable) one reads as switched off.
    pub async fn get(&self) -> Result<ImageGenerationSettingsResponse, SystemError> {
        let stored = self.read_stored().await?;
        Ok(self.response(stored).await)
    }

    /// Save the setting.
    ///
    /// Switching it off needs nothing: the values are stored as given, so the
    /// administrator can always turn it off or clear it, even after the provider
    /// was deleted or disabled. Switching it on needs a server that has the
    /// script, an existing and enabled provider (sessions cannot use a disabled
    /// one) and a model. The model is not checked against the provider's stored
    /// model list: the settings page also offers image models no provider lists.
    pub async fn update(
        &self,
        req: UpdateImageGenerationSettingsRequest,
    ) -> Result<ImageGenerationSettingsResponse, SystemError> {
        let stored = StoredImageGeneration {
            provider_id: non_blank(req.provider_id),
            model: non_blank(req.model),
            enabled: req.enabled,
        };
        if stored.enabled {
            self.ensure_can_be_enabled(&stored).await?;
        }

        let value = serde_json::to_string(&stored)
            .map_err(|e| SystemError::Internal(format!("Failed to serialize image generation setting: {e}")))?;
        self.settings.set(IMAGE_GENERATION_SETTING_KEY, &value).await?;
        info!(
            provider_id = ?stored.provider_id,
            model = ?stored.model,
            enabled = stored.enabled,
            "image generation setting updated"
        );
        if stored.enabled {
            // The first session after the save should find Node ready.
            self.node.warm_up();
        }
        Ok(self.response(stored).await)
    }

    /// The stdio MCP server every session gets while image generation is on.
    ///
    /// `None` when it is off or cannot run: the script is not installed, the
    /// provider is gone or disabled, no model is chosen, the provider's key
    /// cannot be decrypted or there is no Node to run the script with. Each
    /// reason other than "off" is logged, and a session simply starts without
    /// the tool; a broken setting must never stop a conversation.
    ///
    /// This runs whenever a session is assembled, so it never waits more than a
    /// few seconds for Node: a runtime that is still being looked for (or
    /// installed) when the wait is over means no tool for this session, and the
    /// next one that comes after the lookup finished gets it.
    pub async fn session_server(&self) -> Option<SessionMcpServer> {
        let stored = match self.read_stored().await {
            Ok(stored) => stored,
            Err(error) => {
                warn!(%error, "image generation: could not read the setting; sessions start without the tool");
                return None;
            }
        };
        if !stored.enabled {
            debug!("image generation is off; sessions start without the tool");
            return None;
        }
        let (Some(provider_id), Some(model)) = (stored.provider_id.as_deref(), stored.model.as_deref()) else {
            warn!(
                provider_id = ?stored.provider_id,
                model = ?stored.model,
                "image generation is on but has no provider or model; sessions start without the tool"
            );
            return None;
        };
        let Some(script) = self.installed_script().await else {
            warn!("image generation is on but its MCP script is not installed; sessions start without the tool");
            return None;
        };
        let provider = match self.providers.find_by_id(provider_id).await {
            Ok(Some(provider)) => provider,
            Ok(None) => {
                warn!(%provider_id, "image generation provider no longer exists; sessions start without the tool");
                return None;
            }
            Err(error) => {
                warn!(%provider_id, %error, "image generation: could not load the provider; sessions start without the tool");
                return None;
            }
        };
        if !provider.enabled {
            warn!(%provider_id, "image generation provider is disabled; sessions start without the tool");
            return None;
        }
        let api_key = match decrypt_string(&provider.api_key_encrypted, &self.encryption_key) {
            Ok(api_key) => api_key,
            Err(error) => {
                warn!(%provider_id, %error, "image generation provider key cannot be decrypted; sessions start without the tool");
                return None;
            }
        };
        let Some(node) = self.node.get().await else {
            // The cache has said why, once, when the lookup failed or ran long.
            return None;
        };

        // The runtime's own environment first, so the setting always wins.
        let mut env: HashMap<String, String> = node
            .env
            .iter()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect();
        env.extend([
            (ENV_PROVIDER_ID.to_owned(), provider.id.clone()),
            (ENV_PLATFORM.to_owned(), provider.platform.clone()),
            (ENV_BASE_URL.to_owned(), provider.base_url.clone()),
            // As stored: it may be several keys the script rotates between.
            (ENV_API_KEY.to_owned(), api_key),
            (ENV_MODEL.to_owned(), model.to_owned()),
        ]);
        let mut args: Vec<String> = node
            .args_prefix
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        args.push(script.to_string_lossy().into_owned());

        Some(SessionMcpServer {
            id: IMAGE_GENERATION_MCP_NAME.to_owned(),
            name: IMAGE_GENERATION_MCP_NAME.to_owned(),
            transport: SessionMcpTransport::Stdio {
                command: node.program.to_string_lossy().into_owned(),
                args,
                env,
            },
        })
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    async fn ensure_can_be_enabled(&self, stored: &StoredImageGeneration) -> Result<(), SystemError> {
        if !self.supported().await {
            return Err(SystemError::BadRequest(
                "Image generation is not available on this server".into(),
            ));
        }
        let Some(provider_id) = stored.provider_id.as_deref() else {
            return Err(SystemError::BadRequest(
                "A provider is required to turn image generation on".into(),
            ));
        };
        let Some(provider) = self.providers.find_by_id(provider_id).await? else {
            return Err(SystemError::BadRequest(format!("Provider '{provider_id}' not found")));
        };
        if !provider.enabled {
            return Err(SystemError::BadRequest(format!("Provider '{provider_id}' is disabled")));
        }
        if stored.model.is_none() {
            return Err(SystemError::BadRequest(
                "A model is required to turn image generation on".into(),
            ));
        }
        Ok(())
    }

    /// The stored setting; a missing row, or one that does not parse, is the
    /// default (off). A damaged row is logged and left for the administrator's
    /// next save to repair, so it never fails the page or a session. Blank ids
    /// and models read as unset, whoever wrote them.
    async fn read_stored(&self) -> Result<StoredImageGeneration, SystemError> {
        let Some(raw) = self.settings.get(IMAGE_GENERATION_SETTING_KEY).await? else {
            return Ok(StoredImageGeneration::default());
        };
        match serde_json::from_str::<StoredImageGeneration>(&raw) {
            Ok(stored) => Ok(StoredImageGeneration {
                provider_id: non_blank(stored.provider_id),
                model: non_blank(stored.model),
                enabled: stored.enabled,
            }),
            Err(error) => {
                warn!(%error, "image generation setting is damaged; treating it as unset");
                Ok(StoredImageGeneration::default())
            }
        }
    }

    async fn response(&self, stored: StoredImageGeneration) -> ImageGenerationSettingsResponse {
        ImageGenerationSettingsResponse {
            provider_id: stored.provider_id,
            model: stored.model,
            enabled: stored.enabled,
            supported: self.supported().await,
        }
    }

    /// The script path while a file is really there.
    async fn installed_script(&self) -> Option<&Path> {
        let script = self.script.as_deref()?;
        is_file(script).await.then_some(script)
    }
}

/// Trim a value; blank means unset.
fn non_blank(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

async fn is_file(path: &Path) -> bool {
    tokio::fs::metadata(path).await.is_ok_and(|metadata| metadata.is_file())
}

/// Find the Node the backend runs its own tools with (the bundled runtime in a
/// packaged install).
async fn locate_node() -> Option<ResolvedCommand> {
    match aionui_runtime::ensure_runtime_command("node").await {
        Ok(command) => Some(command),
        Err(error) => {
            warn!(%error, "image generation: could not find a Node runtime");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_blank_trims_and_drops_empty_values() {
        assert_eq!(non_blank(None), None);
        assert_eq!(non_blank(Some(String::new())), None);
        assert_eq!(non_blank(Some("  \t\n".to_owned())), None);
        assert_eq!(
            non_blank(Some("  gpt-image-1 ".to_owned())).as_deref(),
            Some("gpt-image-1")
        );
    }

    #[test]
    fn the_stored_setting_reads_partial_values_as_off() {
        let stored: StoredImageGeneration = serde_json::from_str("{}").unwrap();
        assert_eq!(stored, StoredImageGeneration::default());
        let stored: StoredImageGeneration = serde_json::from_str(r#"{"model":"m","unknown":1}"#).unwrap();
        assert_eq!(stored.model.as_deref(), Some("m"));
        assert!(!stored.enabled);
    }

    /// The script reads exactly these names; renaming one silently breaks image
    /// generation, so they are pinned here.
    #[test]
    fn the_script_environment_names_are_the_ones_the_script_reads() {
        assert_eq!(ENV_PROVIDER_ID, "AIONUI_IMG_PROVIDER_ID");
        assert_eq!(ENV_PLATFORM, "AIONUI_IMG_PLATFORM");
        assert_eq!(ENV_BASE_URL, "AIONUI_IMG_BASE_URL");
        assert_eq!(ENV_API_KEY, "AIONUI_IMG_API_KEY");
        assert_eq!(ENV_MODEL, "AIONUI_IMG_MODEL");
        assert_eq!(IMAGE_GENERATION_SETTING_KEY, "tools.imageGeneration");
    }
}
