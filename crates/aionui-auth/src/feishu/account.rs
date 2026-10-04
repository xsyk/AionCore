//! Feishu identity → local account mapping.

use aionui_db::{DbError, FeishuSignupPolicy, IUserRepository, UserStatus, models::User};

use super::client::FeishuUser;
use super::{FEISHU_EXTERNAL_PREFIX, FeishuLoginError};
use crate::password::{generate_password, hash_password};

/// Same limit `/login` enforces, so a Feishu username stays usable there too.
const MAX_USERNAME_BYTES: usize = 32;

/// Longest prefix of `s` that fits in `max` bytes without splitting a char.
pub fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Last `n` bytes of `s`, widened to a char boundary.
fn tail(s: &str, n: usize) -> &str {
    let mut start = s.len().saturating_sub(n);
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// Usernames to try in order: name, then three union-id-suffixed fallbacks.
pub fn username_candidates(user: &FeishuUser) -> Vec<String> {
    let union = user.union_id.as_str();
    let base = [user.name.trim(), user.en_name.trim()]
        .into_iter()
        .find(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("feishu-{}", tail(union, 6)));
    vec![
        truncate_bytes(&base, MAX_USERNAME_BYTES).to_owned(),
        format!("{}-{}", truncate_bytes(&base, 27), tail(union, 4)),
        format!("{}-{}", truncate_bytes(&base, 23), tail(union, 8)),
        truncate_bytes(&format!("feishu-{}", tail(union, 12)), MAX_USERNAME_BYTES).to_owned(),
    ]
}

fn server(e: impl std::fmt::Display) -> FeishuLoginError {
    FeishuLoginError::Server(e.to_string())
}

/// Find the live account for this Feishu identity or create one under
/// `policy`; refresh the profile, then admit only active accounts.
///
/// A soft-deleted account is never revived: the identity gets a fresh account.
/// A disabled account that never logged in is still waiting for approval.
pub async fn resolve_account(
    repo: &dyn IUserRepository,
    user: &FeishuUser,
    policy: FeishuSignupPolicy,
) -> Result<User, FeishuLoginError> {
    let external_id = format!("{FEISHU_EXTERNAL_PREFIX}{}", user.union_id);
    let account = match repo
        .find_live_local_by_external_id(&external_id)
        .await
        .map_err(server)?
    {
        Some(existing) => existing,
        None => create(repo, &external_id, user, initial_status(policy)).await?,
    };
    refresh_profile(repo, &account, user).await;
    let account = repo
        .find_by_id(&account.id)
        .await
        .map_err(server)?
        .ok_or_else(|| server("account vanished"))?;
    match (account.status, account.last_login) {
        (UserStatus::Active, _) => Ok(account),
        (UserStatus::Disabled, None) => {
            tracing::info!(user_id = %account.id, "feishu: account pending approval");
            Err(FeishuLoginError::PendingApproval)
        }
        (UserStatus::Disabled, Some(_)) => Err(FeishuLoginError::AccountDisabled),
    }
}

fn initial_status(policy: FeishuSignupPolicy) -> UserStatus {
    match policy {
        FeishuSignupPolicy::Approval => UserStatus::Disabled,
        FeishuSignupPolicy::Open => UserStatus::Active,
    }
}

async fn create(
    repo: &dyn IUserRepository,
    external_id: &str,
    user: &FeishuUser,
    status: UserStatus,
) -> Result<User, FeishuLoginError> {
    // Nobody knows this password: Feishu users sign in through Feishu only,
    // unless the super admin later resets a password for them.
    let secret = generate_password(32);
    let hash = tokio::task::spawn_blocking(move || hash_password(&secret))
        .await
        .map_err(server)?
        .map_err(server)?;
    for candidate in username_candidates(user) {
        match repo
            .create_external_local_user(external_id, &candidate, &hash, status)
            .await
        {
            Ok(created) => {
                tracing::info!(user_id = %created.id, status = status.as_str(), "feishu: account created");
                return Ok(created);
            }
            Err(DbError::Conflict(_)) => {
                // A concurrent login may have created this identity first.
                if let Some(existing) = repo.find_live_local_by_external_id(external_id).await.map_err(server)? {
                    return Ok(existing);
                }
            }
            Err(e) => return Err(server(e)),
        }
    }
    Err(server("no free username"))
}

async fn refresh_profile(repo: &dyn IUserRepository, account: &User, user: &FeishuUser) {
    let email = match user.email.as_deref() {
        Some(email) => match repo.email_taken(email, &account.id).await {
            Ok(false) => Some(email),
            Ok(true) => None,
            Err(e) => {
                tracing::warn!(error = %e, "feishu: email check failed");
                None
            }
        },
        None => None,
    };
    if let Err(e) = repo
        .update_profile(&account.id, email, user.avatar_url.as_deref())
        .await
    {
        tracing::warn!(user_id = %account.id, error = %e, "feishu: profile refresh failed");
    }
}
