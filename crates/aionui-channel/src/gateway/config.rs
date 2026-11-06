use std::fmt;
use std::io::Read;
use std::path::Path;

use serde::Deserialize;

use crate::error::ChannelError;

/// Private server configuration. The node token must never enter browser state.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    pub url: String,
    pub node_id: String,
    pub token: String,
    pub app_id: String,
    pub tenant_key: String,
    #[serde(default)]
    pub identity: Option<GatewayIdentityConfig>,
}

/// Provisioned independently from Begin responses. Not a trust-on-first-use key.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayIdentityConfig {
    pub verification_key: String,
    pub key_id: String,
}

impl fmt::Debug for GatewayConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GatewayConfig")
            .field("node_id", &self.node_id)
            .field("app_id", &self.app_id)
            .finish_non_exhaustive()
    }
}

impl GatewayConfig {
    pub fn from_env() -> Result<Option<Self>, ChannelError> {
        match std::env::var_os("AION_FEISHU_ROUTE_CONFIG") {
            None => Ok(None),
            Some(path) if path.is_empty() => Err(invalid()),
            Some(path) => Self::read(Path::new(&path)).map(Some),
        }
    }

    fn read(path: &Path) -> Result<Self, ChannelError> {
        let before = std::fs::symlink_metadata(path).map_err(|_| invalid())?;
        if !before.is_file() || before.file_type().is_symlink() || before.len() > 65536 {
            return Err(invalid());
        }
        let mut file = std::fs::File::open(path).map_err(|_| invalid())?;
        let opened = file.metadata().map_err(|_| invalid())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if before.dev() != opened.dev()
                || before.ino() != opened.ino()
                || opened.mode() & 0o077 != 0
                || opened.nlink() != 1
            {
                return Err(invalid());
            }
        }
        if !opened.is_file() || opened.len() > 65536 {
            return Err(invalid());
        }
        let mut data = Vec::new();
        (&mut file).take(65537).read_to_end(&mut data).map_err(|_| invalid())?;
        if data.len() > 65536 {
            return Err(invalid());
        }
        let config: Self = serde_json::from_slice(&data).map_err(|_| invalid())?;
        config.validate()?;
        Ok(config)
    }

    pub(super) fn validate(&self) -> Result<reqwest::Url, ChannelError> {
        let url = reqwest::Url::parse(&self.url).map_err(|_| invalid())?;
        let loopback = url
            .host_str()
            .and_then(|host| host.trim_matches(['[', ']']).parse::<std::net::IpAddr>().ok())
            .is_some_and(|ip| ip.is_loopback());
        if url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !(url.scheme() == "https" || url.scheme() == "http" && loopback)
            || self.token.len() < 32
            || self.token.len() > 4096
            || !self.token.bytes().all(|byte| byte.is_ascii_graphic())
            || !valid_identity(&self.node_id)
            || !valid_identity(&self.app_id)
            || !valid_identity(&self.tenant_key)
        {
            return Err(invalid());
        }
        Ok(url)
    }
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_whitespace)
}

fn invalid() -> ChannelError {
    ChannelError::InvalidConfig("FEISHU_GATEWAY_CONFIG_INVALID".into())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

    struct PrivateConfig(std::path::PathBuf);
    impl Drop for PrivateConfig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn private_config() -> PrivateConfig {
        let mut random = [0u8; 8];
        getrandom::getrandom(&mut random).unwrap();
        let directory = std::env::temp_dir().join(format!("aion-gateway-config-{}", u64::from_ne_bytes(random)));
        std::fs::DirBuilder::new().mode(0o700).create(&directory).unwrap();
        let value = serde_json::json!({"url":"https://route.example.com","node_id":"node_fixture","token":"fixture-node-token-more-than-32-characters","app_id":"cli_fixture","tenant_key":"tenant_fixture"});
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(directory.join("config.json"))
            .unwrap();
        serde_json::to_writer(file, &value).unwrap();
        PrivateConfig(directory)
    }

    #[test]
    fn credential_configuration_requires_private_file_permissions() {
        let fixture = private_config();
        let path = fixture.0.join("config.json");
        assert!(GatewayConfig::read(&path).is_ok());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(GatewayConfig::read(&path).is_err());
    }

    #[test]
    fn credential_configuration_rejects_symbolic_and_hard_links() {
        let fixture = private_config();
        let path = fixture.0.join("config.json");
        let link = fixture.0.join("link.json");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(GatewayConfig::read(&link).is_err());
        std::fs::hard_link(&path, fixture.0.join("hard.json")).unwrap();
        assert!(GatewayConfig::read(&path).is_err());
    }

    #[test]
    fn malformed_configuration_never_echoes_credential_contents() {
        let fixture = private_config();
        let path = fixture.0.join("config.json");
        std::fs::write(&path, b"credential-content-that-is-not-json").unwrap();
        assert!(
            !GatewayConfig::read(&path)
                .unwrap_err()
                .to_string()
                .contains("credential-content")
        );
    }
}
