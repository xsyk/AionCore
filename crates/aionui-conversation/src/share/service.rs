use std::{path::PathBuf, sync::Arc};

use aionui_api_types::{
    ConversationShareStatus, MessageListResponse, PublicShareMessagesQuery, PublicSharedConversation,
    ShareResourceQuery,
};
use aionui_db::{DbError, IConversationShareRepository, MessagePageCursor, MessagePageDirection, MessagePageParams};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

use super::resources;
use crate::{
    convert::row_to_message_response,
    message_cursor::{decode_message_cursor, encode_message_cursor},
};

#[derive(Debug, thiserror::Error)]
pub enum ShareError {
    #[error("Shared conversation is unavailable")]
    NotFound,
    #[error("{0}")]
    BadRequest(&'static str),
    #[error("Share storage operation failed")]
    Storage(#[from] DbError),
    #[error("Share service is unavailable")]
    Internal,
}

pub struct SharedResource {
    pub path: PathBuf,
    pub filename: String,
    pub content_type: String,
    pub attachment: bool,
}

#[derive(Clone)]
pub struct ConversationShareService {
    repo: Arc<dyn IConversationShareRepository>,
}

impl ConversationShareService {
    pub fn new(repo: Arc<dyn IConversationShareRepository>) -> Self {
        Self { repo }
    }

    pub async fn status(&self, owner_id: &str, conversation_id: &str) -> Result<ConversationShareStatus, ShareError> {
        let share = self
            .repo
            .get_for_owner(owner_id, conversation_id)
            .await
            .map_err(owner_error)?;
        Ok(ConversationShareStatus {
            enabled: share.as_ref().is_some_and(|row| row.enabled),
            token: share.map(|row| row.token),
        })
    }

    pub async fn set_enabled(
        &self,
        owner_id: &str,
        conversation_id: &str,
        enabled: bool,
    ) -> Result<ConversationShareStatus, ShareError> {
        let existing = self
            .repo
            .get_for_owner(owner_id, conversation_id)
            .await
            .map_err(owner_error)?;
        if !enabled && existing.is_none() {
            return Ok(ConversationShareStatus {
                enabled: false,
                token: None,
            });
        }
        let token = if let Some(row) = existing {
            row.token
        } else {
            random_token()?
        };
        let row = self
            .repo
            .set_enabled(owner_id, conversation_id, &token, enabled)
            .await
            .map_err(owner_error)?;
        tracing::info!(
            user_id = owner_id,
            conversation_id,
            enabled,
            "conversation share state changed"
        );
        Ok(ConversationShareStatus {
            enabled: row.enabled,
            token: Some(row.token),
        })
    }

    pub async fn public_conversation(&self, token: &str) -> Result<PublicSharedConversation, ShareError> {
        let row = self.public_parent(token).await?;
        Ok(PublicSharedConversation {
            id: row.id,
            name: row.name,
            r#type: row.r#type,
            created_at: row.created_at,
            modified_at: row.updated_at,
        })
    }

    pub async fn public_messages(
        &self,
        token: &str,
        query: PublicShareMessagesQuery,
    ) -> Result<MessageListResponse, ShareError> {
        validate_token(token)?;
        if query.before.is_some() && query.after.is_some() {
            return Err(ShareError::BadRequest("before and after are mutually exclusive"));
        }
        let direction = if let Some(cursor) = query.before {
            MessagePageDirection::Before {
                cursor: decode_message_cursor(&cursor).map_err(|_| ShareError::BadRequest("invalid message cursor"))?,
            }
        } else if let Some(cursor) = query.after {
            MessagePageDirection::After {
                cursor: decode_message_cursor(&cursor).map_err(|_| ShareError::BadRequest("invalid message cursor"))?,
            }
        } else {
            MessagePageDirection::InitialLatest
        };
        let page = self
            .repo
            .list_public_messages(
                token,
                &MessagePageParams {
                    limit: query.limit.unwrap_or(100).clamp(1, 200),
                    direction,
                },
            )
            .await
            .map_err(owner_error)?;
        let oldest_cursor = page
            .items
            .first()
            .map(|row| encode_message_cursor(&MessagePageCursor::from(row)))
            .transpose()
            .map_err(|_| ShareError::Internal)?;
        let newest_cursor = page
            .items
            .last()
            .map(|row| encode_message_cursor(&MessagePageCursor::from(row)))
            .transpose()
            .map_err(|_| ShareError::Internal)?;
        let items = page
            .items
            .into_iter()
            .map(|row| {
                let mut message = row_to_message_response(row).map_err(|_| ShareError::Internal)?;
                message.backend_turn_id = None;
                Ok(message)
            })
            .collect::<Result<Vec<_>, ShareError>>()?;
        Ok(MessageListResponse {
            items,
            oldest_cursor,
            newest_cursor,
            has_more_before: page.has_more_before,
            has_more_after: page.has_more_after,
        })
    }

    pub async fn public_artifacts(
        &self,
        token: &str,
    ) -> Result<aionui_api_types::ConversationArtifactListResponse, ShareError> {
        validate_token(token)?;
        let mut items = self
            .repo
            .list_public_artifacts(token)
            .await
            .map_err(owner_error)?
            .into_iter()
            .map(crate::convert::row_to_artifact_response)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| ShareError::Internal)?;
        items.extend(
            self.repo
                .list_public_legacy_triggers(token)
                .await
                .map_err(owner_error)?
                .into_iter()
                .filter_map(|row| crate::service::legacy_cron_trigger_to_artifact(row).ok()),
        );
        items.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(items)
    }

    pub async fn resource(&self, token: &str, query: &ShareResourceQuery) -> Result<SharedResource, ShareError> {
        let conversation = self.public_parent(token).await?;
        let extra: serde_json::Value = serde_json::from_str(&conversation.extra).map_err(|_| ShareError::NotFound)?;
        let workspace = extra.get("workspace").and_then(serde_json::Value::as_str);
        let requested = resources::resource_path(&query.path, workspace).ok_or(ShareError::NotFound)?;
        let canonical = tokio::fs::canonicalize(&requested)
            .await
            .map_err(|_| ShareError::NotFound)?;
        let mut roots = Vec::new();
        if let Some(workspace) = workspace.filter(|workspace| !workspace.is_empty()) {
            roots.push(PathBuf::from(workspace));
        }
        let mut contained = false;
        for root in roots {
            if let Ok(root) = tokio::fs::canonicalize(root).await
                && canonical.starts_with(root)
            {
                contained = true;
                break;
            }
        }
        if !contained
            && resources::safe_component(&conversation.id)
            && let Ok(upload_root) = tokio::fs::canonicalize(std::env::temp_dir().join("aionui")).await
        {
            let expected = upload_root.join(&conversation.id);
            if let Ok(actual) = tokio::fs::canonicalize(&expected).await
                && actual == expected
                && canonical.starts_with(actual)
            {
                contained = true;
            }
        }
        if !contained
            || !tokio::fs::metadata(&canonical)
                .await
                .is_ok_and(|metadata| metadata.is_file())
        {
            return Err(ShareError::NotFound);
        }
        let mut direction = MessagePageDirection::InitialLatest;
        let mut referenced = false;
        loop {
            let page = self
                .repo
                .list_public_messages(token, &MessagePageParams { limit: 200, direction })
                .await
                .map_err(owner_error)?;
            for row in &page.items {
                let content: serde_json::Value =
                    serde_json::from_str(&row.content).map_err(|_| ShareError::Internal)?;
                for path in resources::referenced_paths(&row.r#type, &content) {
                    if let Some(path) = resources::resource_path(&path, workspace)
                        && let Ok(path) = tokio::fs::canonicalize(path).await
                        && path == canonical
                    {
                        referenced = true;
                        break;
                    }
                }
                if referenced {
                    break;
                }
            }
            if referenced || !page.has_more_before {
                break;
            }
            let Some(oldest) = page.items.first() else {
                break;
            };
            direction = MessagePageDirection::Before { cursor: oldest.into() };
        }
        if !referenced {
            return Err(ShareError::NotFound);
        }
        // Recheck revocation after the potentially multi-page lookup.
        self.public_parent(token).await?;
        let filename = canonical
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("download")
            .to_owned();
        let mime = mime_guess::from_path(&canonical).first_or_octet_stream();
        let safe_inline = matches!(
            mime.essence_str(),
            "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/avif"
        );
        Ok(SharedResource {
            path: canonical,
            filename,
            content_type: if safe_inline {
                mime.essence_str().to_owned()
            } else {
                "application/octet-stream".into()
            },
            attachment: query.download || !safe_inline,
        })
    }

    async fn public_parent(&self, token: &str) -> Result<aionui_db::models::ConversationRow, ShareError> {
        validate_token(token)?;
        self.repo.find_public(token).await?.ok_or(ShareError::NotFound)
    }
}

fn owner_error(error: DbError) -> ShareError {
    match error {
        DbError::NotFound(_) => ShareError::NotFound,
        error => ShareError::Storage(error),
    }
}

fn random_token() -> Result<String, ShareError> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|_| ShareError::Internal)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn validate_token(token: &str) -> Result<(), ShareError> {
    if token.len() == 43
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        Ok(())
    } else {
        Err(ShareError::NotFound)
    }
}

/// Preserve an authenticated upload's ownership when it becomes a conversation
/// attachment. Historical global uploads are intentionally left unchanged:
/// their path alone is not evidence of the uploader's identity.
pub(crate) async fn scope_upload_attachments(
    user_id: &str,
    conversation_id: &str,
    references: &[aionui_api_types::ChatFileRef],
    resolved: &mut aionui_project::ResolvedChatMessage,
) -> Result<(), crate::ConversationError> {
    scope_uploads_at(
        user_id,
        conversation_id,
        references,
        resolved,
        &std::env::temp_dir().join("aionui"),
    )
    .await
}

async fn scope_uploads_at(
    user_id: &str,
    conversation_id: &str,
    references: &[aionui_api_types::ChatFileRef],
    resolved: &mut aionui_project::ResolvedChatMessage,
    upload_root: &std::path::Path,
) -> Result<(), crate::ConversationError> {
    use aionui_api_types::ChatFileRef;
    use aionui_common::constants::AIONUI_FILES_MARKER;
    use tokio::io::AsyncWriteExt;
    if resolved.files.is_empty() {
        return Ok(());
    }
    if references.len() != resolved.files.len() || !resources::safe_component(conversation_id) {
        return Err(crate::ConversationError::bad_request("invalid attachment context"));
    }
    let namespace = aionui_common::user_upload_dir_name(user_id)
        .map_err(|_| crate::ConversationError::bad_request("invalid attachment account"))?;
    let owner_root = upload_root.join(&namespace);
    let Ok(owner_root) = tokio::fs::canonicalize(&owner_root).await else {
        return Ok(());
    };
    let canonical_upload_root = tokio::fs::canonicalize(upload_root)
        .await
        .map_err(|_| crate::ConversationError::internal("upload storage is unavailable"))?;
    if owner_root != canonical_upload_root.join(&namespace) {
        return Err(crate::ConversationError::bad_request("invalid upload ownership"));
    }
    let mut changed = false;
    for (reference, path) in references.iter().zip(&mut resolved.files) {
        if !matches!(reference, ChatFileRef::Upload { .. }) {
            continue;
        }
        let source = tokio::fs::canonicalize(&*path)
            .await
            .map_err(|_| crate::ConversationError::bad_request("attachment is unavailable"))?;
        if !source.starts_with(&owner_root) {
            continue;
        }
        let filename = source
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.chars().any(char::is_control))
            .ok_or_else(|| crate::ConversationError::bad_request("invalid attachment file name"))?;
        let target_dir = upload_root.join(conversation_id);
        tokio::fs::create_dir_all(&target_dir)
            .await
            .map_err(|_| crate::ConversationError::internal("cannot prepare conversation attachments"))?;
        if tokio::fs::canonicalize(&target_dir).await.ok().as_ref()
            != Some(&canonical_upload_root.join(conversation_id))
        {
            return Err(crate::ConversationError::bad_request(
                "invalid conversation attachment directory",
            ));
        }
        let mut attempts = 0;
        let destination = loop {
            let name = if attempts == 0 {
                filename.to_owned()
            } else {
                format!("{}-{filename}", aionui_common::generate_id())
            };
            let target = target_dir.join(name);
            match tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .await
            {
                Ok(mut output) => {
                    let copy = async {
                        let mut input = tokio::fs::File::open(&source).await?;
                        tokio::io::copy(&mut input, &mut output).await?;
                        output.flush().await
                    }
                    .await;
                    if copy.is_err() {
                        let _ = tokio::fs::remove_file(&target).await;
                        return Err(crate::ConversationError::internal(
                            "cannot preserve conversation attachment",
                        ));
                    }
                    break target;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && attempts < 3 => {
                    attempts += 1;
                }
                Err(_) => {
                    return Err(crate::ConversationError::internal(
                        "cannot preserve conversation attachment",
                    ));
                }
            }
        };
        *path = destination.to_string_lossy().into_owned();
        changed = true;
    }
    if changed {
        let marker = format!("\n\n{AIONUI_FILES_MARKER}\n");
        let head = resolved
            .content
            .rsplit_once(&marker)
            .map(|(head, _)| head)
            .ok_or_else(|| crate::ConversationError::internal("resolved attachment marker missing"))?;
        resolved.content = format!("{head}{marker}{}", resolved.files.join("\n"));
    }
    Ok(())
}

#[cfg(test)]
mod attachment_tests {
    use super::*;

    #[tokio::test]
    async fn owned_upload_is_copied_to_the_conversation_and_other_uploads_are_not_granted() {
        let root = tempfile::tempdir().unwrap();
        let owner = root.path().join("owner_system_default_user");
        tokio::fs::create_dir_all(&owner).await.unwrap();
        let source = owner.join("report.txt");
        tokio::fs::write(&source, b"report").await.unwrap();
        let path = source.to_string_lossy().into_owned();
        let references = vec![aionui_api_types::ChatFileRef::Upload { path: path.clone() }];
        let mut resolved = aionui_project::ResolvedChatMessage {
            content: format!("Read\n\n[[AION_FILES]]\n{path}"),
            files: vec![path.clone()],
        };
        scope_uploads_at(
            "system_default_user",
            "conversation",
            &references,
            &mut resolved,
            root.path(),
        )
        .await
        .unwrap();
        assert!(PathBuf::from(&resolved.files[0]).starts_with(root.path().join("conversation")));
        assert_eq!(tokio::fs::read(&resolved.files[0]).await.unwrap(), b"report");
        assert!(resolved.content.ends_with(&resolved.files[0]));
        let mut untouched = aionui_project::ResolvedChatMessage {
            content: format!("Read\n\n[[AION_FILES]]\n{path}"),
            files: vec![path.clone()],
        };
        scope_uploads_at(
            "other_user",
            "other-conversation",
            &references,
            &mut untouched,
            root.path(),
        )
        .await
        .unwrap();
        assert_eq!(untouched.files, vec![path]);
        assert!(!root.path().join("other-conversation").exists());
    }
}
