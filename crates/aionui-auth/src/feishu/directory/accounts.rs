//! 按飞书稳定身份导入账号，既有权限、登录名和删除标记保持原值。

use aionui_db::{DbError, FeishuSignupPolicy, IUserRepository, UserStatus, UserType, models::User};

use super::client::DirectoryMember;
use super::service::DirectoryError;
use crate::feishu::{FEISHU_EXTERNAL_PREFIX, account::username_candidates, client::FeishuUser};
use crate::password::{generate_password, hash_password};

pub(super) enum ImportOutcome {
    Created,
    CreatedWithProfileFailure,
    Updated,
    Unchanged,
    Skipped,
}

pub(super) async fn import_member(
    repo: &dyn IUserRepository,
    member: &DirectoryMember,
    policy: FeishuSignupPolicy,
) -> Result<ImportOutcome, DirectoryError> {
    let Some(union_id) = member
        .union_id
        .as_deref()
        .filter(|id| !id.is_empty() && id.len() <= 256 && id.trim() == *id && !id.chars().any(char::is_control))
    else {
        return Ok(ImportOutcome::Skipped);
    };
    let external_id = format!("{FEISHU_EXTERNAL_PREFIX}{union_id}");
    let existing = repo
        .find_live_local_by_external_id(&external_id)
        .await
        .map_err(|_| DirectoryError::Failed)?;
    let (account, created) = if let Some(account) = existing {
        (account, false)
    } else {
        // 同步不会让管理员已删除的身份重新出现在用户列表。
        if repo
            .find_by_external_user_id(UserType::Local, &external_id)
            .await
            .map_err(|_| DirectoryError::Failed)?
            .is_some()
        {
            return Ok(ImportOutcome::Skipped);
        }
        create(repo, &external_id, member, union_id, policy).await?
    };
    let updated = match refresh_profile(repo, &account, member).await {
        Ok(updated) => updated,
        Err(_) if created => return Ok(ImportOutcome::CreatedWithProfileFailure),
        Err(error) => return Err(error),
    };
    Ok(if created {
        ImportOutcome::Created
    } else if updated {
        ImportOutcome::Updated
    } else {
        ImportOutcome::Unchanged
    })
}

async fn create(
    repo: &dyn IUserRepository,
    external_id: &str,
    member: &DirectoryMember,
    union_id: &str,
    policy: FeishuSignupPolicy,
) -> Result<(User, bool), DirectoryError> {
    let user = FeishuUser {
        union_id: union_id.into(),
        name: clean_name(member.name.as_deref()),
        en_name: clean_name(member.en_name.as_deref()),
        email: None,
        avatar_url: None,
        tenant_key: String::new(),
    };
    let secret = generate_password(32);
    let hash = tokio::task::spawn_blocking(move || hash_password(&secret))
        .await
        .map_err(|_| DirectoryError::Failed)?
        .map_err(|_| DirectoryError::Failed)?;
    let status = match policy {
        FeishuSignupPolicy::Open => UserStatus::Active,
        FeishuSignupPolicy::Approval => UserStatus::Disabled,
    };
    for username in username_candidates(&user) {
        match repo
            .create_external_local_user(external_id, &username, &hash, status)
            .await
        {
            Ok(user) => return Ok((user, true)),
            Err(DbError::Conflict(_)) => {
                if let Some(user) = repo
                    .find_live_local_by_external_id(external_id)
                    .await
                    .map_err(|_| DirectoryError::Failed)?
                {
                    return Ok((user, false));
                }
            }
            Err(_) => return Err(DirectoryError::Failed),
        }
    }
    Err(DirectoryError::Failed)
}

fn clean_name(value: Option<&str>) -> String {
    value
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

async fn refresh_profile(
    repo: &dyn IUserRepository,
    account: &User,
    member: &DirectoryMember,
) -> Result<bool, DirectoryError> {
    let email = member
        .email
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 320 && !value.chars().any(char::is_control));
    let email = match email {
        Some(email) if account.email.as_deref() != Some(email) => {
            if repo
                .email_taken(email, &account.id)
                .await
                .map_err(|_| DirectoryError::Failed)?
            {
                None
            } else {
                Some(email)
            }
        }
        _ => None,
    };
    let avatar = member
        .avatar
        .as_ref()
        .and_then(|avatar| avatar.avatar_240.as_deref())
        .filter(|value| {
            value.len() <= 2048
                && reqwest::Url::parse(value)
                    .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some())
        })
        .filter(|value| account.avatar_path.as_deref() != Some(*value));
    let display_name = [member.name.as_deref(), member.en_name.as_deref()]
        .into_iter()
        .map(clean_name)
        .find(|name| {
            !name.is_empty()
                && member.open_id.as_deref() != Some(name.as_str())
                && member.union_id.as_deref() != Some(name.as_str())
        })
        .map(|name| name.chars().take(128).collect::<String>())
        .filter(|name| account.display_name.as_deref() != Some(name.as_str()));
    if let Some(name) = display_name.as_deref() {
        repo.update_display_name(&account.id, name)
            .await
            .map_err(|_| DirectoryError::Failed)?;
    }
    if email.is_none() && avatar.is_none() {
        return Ok(display_name.is_some());
    }
    repo.update_profile(&account.id, email, avatar)
        .await
        .map_err(|_| DirectoryError::Failed)?;
    Ok(true)
}
