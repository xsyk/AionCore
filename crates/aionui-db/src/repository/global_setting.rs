use crate::error::DbError;

/// Server-wide settings data access abstraction.
///
/// The `global_settings` table is a plain key-value store for settings that
/// belong to the whole server rather than to one user (for example the image
/// generation model). Values are opaque strings: each caller owns the format of
/// the keys it writes.
#[async_trait::async_trait]
pub trait IGlobalSettingRepository: Send + Sync {
    /// Returns the value stored under `key`, or `None` if it was never set.
    async fn get(&self, key: &str) -> Result<Option<String>, DbError>;

    /// Stores `value` under `key`, replacing any value already there.
    async fn set(&self, key: &str, value: &str) -> Result<(), DbError>;
}
