use std::path::{Path, PathBuf};

use aionui_common::EnvVar;
use aionui_db::IConversationRepository;
use serde::{Deserialize, Deserializer, de};

use super::files::TrustedFiles;
use super::{QaLaunchEnvironment, QaLaunchError, ValidatedQaLaunch, reserved_key, unix_now};

const SCOPE_FILE: &str = "/etc/aioneasiful/qa-session-scope.json";
const BINDING_DIR: &str = "/etc/aioneasiful/qa-session-launch";
const SCOPE_LIMIT: usize = 4096;
const BINDING_LIMIT: usize = 16384;
const MAX_LIFETIME: u64 = 24 * 60 * 60;

type Result<T> = std::result::Result<T, QaLaunchError>;

// Require maps while leaving field validation to the derived deserializer.
// Passing MapAccess directly preserves duplicate keys for that validation.
fn deserialize_object<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct ObjectVisitor<T>(std::marker::PhantomData<T>);

    impl<'de, T: Deserialize<'de>> de::Visitor<'de> for ObjectVisitor<T> {
        type Value = T;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a JSON object")
        }

        fn visit_map<A>(self, map: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: de::MapAccess<'de>,
        {
            T::deserialize(de::value::MapAccessDeserializer::new(map))
        }
    }

    deserializer.deserialize_map(ObjectVisitor::<T>(std::marker::PhantomData))
}

fn from_json_object<'de, T: Deserialize<'de>>(bytes: &'de [u8]) -> std::result::Result<T, serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = deserialize_object(&mut deserializer)?;
    deserializer.end()?;
    Ok(value)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Scope {
    version: u32,
    #[serde(deserialize_with = "deserialize_object")]
    target: Target,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    conversation_id: String,
    user_id: String,
    backend: Backend,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(try_from = "String")]
enum Backend {
    Codex,
}

impl TryFrom<String> for Backend {
    type Error = &'static str;

    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        match value.as_str() {
            "codex" => Ok(Self::Codex),
            _ => Err("expected codex"),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    version: u32,
    conversation_id: String,
    user_id: String,
    backend: Backend,
    expires_at: u64,
    #[serde(deserialize_with = "deserialize_object")]
    env: QaEnv,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QaEnv {
    #[serde(rename = "AION_QA_CODEX_PROJECTION_BINARY")]
    binary: String,
    #[serde(rename = "AION_QA_CODEX_PROJECTION_EXPECT_EXE_SHA256")]
    sha256: String,
    #[serde(rename = "AION_QA_CODEX_PROJECTION_DIRECTORY")]
    directory: String,
    #[serde(rename = "AION_QA_CODEX_PROJECTION_NONCE_FILE")]
    nonce_file: String,
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn canonical_absolute(value: &str) -> bool {
    value.starts_with('/')
        && value.len() <= 4096
        && !value.contains('\0')
        && value[1..]
            .split('/')
            .all(|component| !component.is_empty() && component != "." && component != "..")
}

pub(super) struct Policy {
    // Owned, immutable, loaded only once. Never derive membership from bindings.
    scope: Scope,
    files: TrustedFiles,
    binding_dir: PathBuf,
}

impl Policy {
    pub(super) fn from_startup() -> Result<Self> {
        Self::load(
            TrustedFiles::production()?,
            Path::new(SCOPE_FILE),
            PathBuf::from(BINDING_DIR),
        )
    }

    fn load(files: TrustedFiles, scope_file: &Path, binding_dir: PathBuf) -> Result<Self> {
        let load = || {
            let bytes = files.read_admin(scope_file, SCOPE_LIMIT)?;
            let scope: Scope = from_json_object(&bytes).map_err(|_| QaLaunchError("QA_SCOPE_CONFIG_INVALID"))?;
            if scope.version != 1 || !valid_id(&scope.target.conversation_id) || !valid_id(&scope.target.user_id) {
                return Err(QaLaunchError("QA_SCOPE_CONFIG_INVALID"));
            }
            let Backend::Codex = scope.target.backend;
            Ok(scope)
        };
        let scope = load().map_err(|_| QaLaunchError("QA_SCOPE_CONFIG_INVALID"))?;
        Ok(Self {
            scope,
            files,
            binding_dir,
        })
    }

    pub(super) async fn resolve(
        &self,
        backend: &str,
        conversation_id: &str,
        user_id: &str,
        repo: Option<&dyn IConversationRepository>,
        env: QaLaunchEnvironment<'_>,
        is_fork: bool,
    ) -> Result<Option<ValidatedQaLaunch>> {
        // This check MUST precede database and filesystem access. An inaccessible
        // binding directory must never affect an unrelated conversation/backend.
        if conversation_id != self.scope.target.conversation_id || backend != "codex" {
            return Ok(None);
        }
        if user_id != self.scope.target.user_id {
            return Err(QaLaunchError("QA_OWNER_MISMATCH"));
        }
        if is_fork {
            return Err(QaLaunchError("QA_FORK_REJECTED"));
        }
        let repo = repo.ok_or(QaLaunchError("QA_OWNER_UNAVAILABLE"))?;
        let owner = repo
            .owner_user_id(conversation_id)
            .await
            .map_err(|_| QaLaunchError("QA_OWNER_UNAVAILABLE"))?;
        if owner.as_deref() != Some(user_id) {
            return Err(QaLaunchError("QA_OWNER_MISMATCH"));
        }
        let row = repo
            .get(user_id, conversation_id)
            .await
            .map_err(|_| QaLaunchError("QA_OWNER_UNAVAILABLE"))?;
        if !row.is_some_and(|row| row.id == conversation_id && row.user_id == user_id) {
            return Err(QaLaunchError("QA_OWNER_MISMATCH"));
        }
        if env.metadata_env.iter().any(|entry| reserved_key(&entry.name))
            || env.runtime_env.iter().any(|(name, _)| reserved_key(name))
        {
            return Err(QaLaunchError("QA_RESERVED_ENV_CONFLICT"));
        }
        // The immutable scope ID was validated before it became a filename.
        let path = self.binding_dir.join(format!("{conversation_id}.json"));
        let bytes = self
            .files
            .read_admin(&path, BINDING_LIMIT)
            .map_err(|_| QaLaunchError("QA_BINDING_UNAVAILABLE"))?;
        let binding: Binding = from_json_object(&bytes).map_err(|_| QaLaunchError("QA_BINDING_INVALID"))?;
        let Backend::Codex = binding.backend;
        if binding.version != 1 || binding.conversation_id != conversation_id || binding.user_id != user_id {
            return Err(QaLaunchError("QA_BINDING_IDENTITY_MISMATCH"));
        }
        let now = unix_now()?;
        if binding.expires_at <= now || binding.expires_at > now.saturating_add(MAX_LIFETIME) {
            return Err(QaLaunchError("QA_BINDING_EXPIRED"));
        }
        let env = binding.env;
        if !canonical_absolute(&env.binary)
            || !canonical_absolute(&env.directory)
            || !canonical_absolute(&env.nonce_file)
            || env.sha256.len() != 64
            || !env
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(QaLaunchError("QA_BINDING_ENV_INVALID"));
        }
        let driver = self.binding_dir.join("driver");
        let program = driver.join("launch_qa.py");
        self.files
            .check_driver(&program)
            .map_err(|_| QaLaunchError("QA_DRIVER_REJECTED"))?;
        self.files
            .check_driver(&driver.join("probe.py"))
            .map_err(|_| QaLaunchError("QA_DRIVER_REJECTED"))?;
        self.files
            .check_binary(Path::new(&env.binary))
            .map_err(|_| QaLaunchError("QA_BINARY_REJECTED"))?;
        Ok(Some(ValidatedQaLaunch {
            conversation_id: conversation_id.to_owned(),
            user_id: user_id.to_owned(),
            program,
            env: [
                EnvVar {
                    name: "AION_QA_CODEX_PROJECTION_BINARY".into(),
                    value: env.binary,
                },
                EnvVar {
                    name: "AION_QA_CODEX_PROJECTION_EXPECT_EXE_SHA256".into(),
                    value: env.sha256,
                },
                EnvVar {
                    name: "AION_QA_CODEX_PROJECTION_DIRECTORY".into(),
                    value: env.directory,
                },
                EnvVar {
                    name: "AION_QA_CODEX_PROJECTION_NONCE_FILE".into(),
                    value: env.nonce_file,
                },
            ],
            expires_at: binding.expires_at,
        }))
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
