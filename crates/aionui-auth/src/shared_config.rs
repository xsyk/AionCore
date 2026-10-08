#![allow(clippy::disallowed_types)]

//! Shared (server-wide) configuration — model providers, agents and the image
//! generation model — is managed by the super admin and used by every user.

use aionui_common::ApiError;
use aionui_common::constants::is_super_admin;

use crate::middleware::{CurrentUser, RealUser};

/// Gate for writes to shared configuration.
///
/// Authorizes on the *real* caller, so the super admin may change shared
/// settings while acting as another user (the settings are not per-user).
/// Requests that never passed the auth middleware's act-as step carry no
/// [`RealUser`]; they fall back to the effective user.
pub fn require_shared_config_admin(real: Option<&RealUser>, current: &CurrentUser) -> Result<(), ApiError> {
    if can_manage_shared_config(real, current) {
        Ok(())
    } else {
        Err(ApiError::Forbidden(
            "Only the administrator can change shared settings".into(),
        ))
    }
}

/// Whether the caller manages shared configuration (and may see its secrets).
pub fn can_manage_shared_config(real: Option<&RealUser>, current: &CurrentUser) -> bool {
    let caller = real.map_or(current.id.as_str(), |real| real.0.id.as_str());
    is_super_admin(caller)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_common::constants::SUPER_ADMIN_USER_ID;
    use aionui_db::{UserStatus, UserType};

    fn user(id: &str) -> CurrentUser {
        CurrentUser {
            id: id.to_owned(),
            username: id.to_owned(),
            user_type: UserType::Local,
            status: UserStatus::Active,
        }
    }

    #[test]
    fn the_super_admin_manages_shared_config() {
        assert!(require_shared_config_admin(None, &user(SUPER_ADMIN_USER_ID)).is_ok());
    }

    #[test]
    fn other_users_are_refused() {
        let err = require_shared_config_admin(None, &user("user_1")).unwrap_err();
        assert!(matches!(err, ApiError::Forbidden(_)));
    }

    #[test]
    fn the_super_admin_acting_as_someone_else_still_manages_shared_config() {
        let real = RealUser(user(SUPER_ADMIN_USER_ID));
        assert!(require_shared_config_admin(Some(&real), &user("user_1")).is_ok());
    }

    #[test]
    fn the_real_caller_decides_not_the_effective_user() {
        let real = RealUser(user("user_1"));
        assert!(!can_manage_shared_config(Some(&real), &user(SUPER_ADMIN_USER_ID)));
    }
}
