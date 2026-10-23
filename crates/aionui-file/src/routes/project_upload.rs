//! `POST /api/fs/upload-to-project` — upload one file into a directory of a project
//! entry (the Explorer's "upload file").
//!
//! Multipart contract, in order: the `pe_id` and `relative_path` text parts
//! (`relative_path` may be `""`, the entry root) plus an optional `file_name`, then
//! exactly one `file` part, which must be the last part. The name is the last
//! segment of `file_name`, else of the `file` part's Content-Disposition filename.
//! The target is resolved and checked before a byte is written, and the content is
//! streamed to disk chunk by chunk; the upload is committed only after the whole
//! body has been read, so every failure leaves nothing behind.

use std::path::PathBuf;

use axum::Json;
use axum::extract::multipart::{Field, MultipartError, MultipartRejection};
use axum::extract::{Extension, Multipart, State};
use axum::http::StatusCode;

use aionui_api_types::{ApiResponse, UploadToProjectResponse};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;

use super::{FileRouterState, sanitize_upload_filename};
use crate::service::ProjectUpload;

/// Longest accepted text part (`pe_id`, `relative_path`, `file_name`), in bytes.
/// Text parts are buffered whole, so they get a bound far below the body limit.
const MAX_TEXT_PART_BYTES: usize = 8 * 1024;

pub(super) async fn upload_to_project(
    State(state): State<FileRouterState>,
    Extension(user): Extension<CurrentUser>,
    multipart: Result<Multipart, MultipartRejection>,
) -> Result<Json<ApiResponse<UploadToProjectResponse>>, ApiError> {
    let mut pe_id = None;
    let result = receive_upload(&state, &user, multipart, &mut pe_id).await;
    let pe_id = pe_id.as_deref().unwrap_or_default();
    match result {
        Ok(uploaded) => {
            tracing::debug!(
                user_id = %user.id,
                pe_id,
                relative_path = %uploaded.relative_path,
                size = uploaded.size,
                renamed = uploaded.renamed,
                "fs: file uploaded into project"
            );
            Ok(Json(ApiResponse::ok(uploaded)))
        }
        Err(err) => {
            let code = err.error_code();
            if err.status_code().is_server_error() {
                tracing::error!(user_id = %user.id, pe_id, code, reason = %err, "fs: upload into project failed");
            } else {
                tracing::warn!(user_id = %user.id, pe_id, code, reason = %err, "fs: upload into project rejected");
            }
            Err(err)
        }
    }
}

/// Walk the parts in contract order and stream the `file` part to disk. `pe_id` is
/// filled in as soon as it is read, for the caller's log line.
async fn receive_upload(
    state: &FileRouterState,
    user: &CurrentUser,
    multipart: Result<Multipart, MultipartRejection>,
    pe_id: &mut Option<String>,
) -> Result<UploadToProjectResponse, ApiError> {
    let mut multipart =
        multipart.map_err(|_| ApiError::BadRequest("expected a multipart/form-data body".to_owned()))?;
    let mut relative_path: Option<String> = None;
    let mut file_name: Option<String> = None;

    while let Some(field) = multipart.next_field().await.map_err(multipart_error)? {
        let part = field.name().unwrap_or_default().to_owned();
        match part.as_str() {
            "pe_id" => set_once(pe_id, "pe_id", read_text_part(field, "pe_id").await?)?,
            "relative_path" => set_once(
                &mut relative_path,
                "relative_path",
                read_text_part(field, "relative_path").await?,
            )?,
            "file_name" => set_once(&mut file_name, "file_name", read_text_part(field, "file_name").await?)?,
            "file" => {
                let (Some(pe), Some(relative_path)) = (pe_id.clone(), relative_path.take()) else {
                    return Err(ApiError::BadRequest(
                        "'pe_id' and 'relative_path' must precede 'file'".to_owned(),
                    ));
                };
                if pe.trim().is_empty() {
                    return Err(ApiError::BadRequest("'pe_id' must not be empty".to_owned()));
                }
                // A blank `file_name` counts as absent.
                let raw_name = file_name
                    .take()
                    .filter(|name| !name.trim().is_empty())
                    .or_else(|| field.file_name().map(str::to_owned))
                    .ok_or_else(|| {
                        ApiError::BadRequest(
                            "missing file name: send a filename with the 'file' part or a 'file_name' part".to_owned(),
                        )
                    })?;
                let requested = sanitize_upload_filename(&raw_name)
                    .ok_or_else(|| ApiError::BadRequest("invalid file name".to_owned()))?;

                let target = resolve_target(state, user, pe, relative_path).await?;
                let upload = stream_file(state, &target, &requested, field).await?;

                // `file` must be the last part. The upload is not committed yet, so
                // returning here removes the file it wrote.
                if let Some(extra) = multipart.next_field().await.map_err(multipart_error)? {
                    let message = if extra.name() == Some("file") {
                        "only one 'file' part is allowed"
                    } else {
                        "'file' must be the last part"
                    };
                    return Err(ApiError::BadRequest(message.to_owned()));
                }

                let uploaded = upload.finish().await?;
                return Ok(UploadToProjectResponse {
                    relative_path: join_relative(&target.relative_path, &uploaded.name),
                    name: uploaded.name,
                    size: uploaded.size,
                    renamed: uploaded.renamed,
                });
            }
            // Other parts ahead of `file` are skipped unread.
            _ => {}
        }
    }
    Err(ApiError::BadRequest("missing 'file' part".to_owned()))
}

/// The directory an upload lands in, resolved from its pe-addressed reference.
struct UploadTarget {
    /// The entry's folder root.
    root: PathBuf,
    /// The target directory: `root` joined with `relative_path`.
    dir: PathBuf,
    /// Normalized by `resolve_reference`: `/`-separated, no leading or trailing
    /// slash, `""` for the root.
    relative_path: String,
}

/// Resolve the reference the way the Explorer's own write commands do: ownership
/// plus lexical containment, with errors mapped through the project mapping (an
/// entry of another user is `project_explorer_not_found`). The file service then
/// checks the directory on disk.
async fn resolve_target(
    state: &FileRouterState,
    user: &CurrentUser,
    pe_id: String,
    relative_path: String,
) -> Result<UploadTarget, ApiError> {
    let resolved = state
        .project
        .resolve_reference(
            &user.id,
            aionui_project::ReferenceInput {
                pe_id,
                relative_path,
                op: aionui_project::FileOp::Write,
            },
        )
        .await
        .map_err(ApiError::from)?;
    let dir = resolved
        .absolute_path
        .ok_or_else(|| ApiError::BadRequest("upload target is not a local path".to_owned()))?;
    // `absolute_path` is built on the real-case root URI, so that is the root it
    // has to stay within.
    let root = aionui_project::canonical::uri_to_path(&resolved.root_resource_uri)
        .map_err(|_| ApiError::Internal("cannot resolve the project folder".to_owned()))?;
    Ok(UploadTarget {
        root,
        dir: PathBuf::from(dir),
        relative_path: resolved.relative_path,
    })
}

/// Reserve the file and copy the part's content into it as it arrives.
async fn stream_file(
    state: &FileRouterState,
    target: &UploadTarget,
    file_name: &str,
    mut field: Field<'_>,
) -> Result<ProjectUpload, ApiError> {
    let mut upload = state
        .file_service
        .begin_project_upload(&target.root, &target.dir, file_name)
        .await?;
    while let Some(chunk) = field.chunk().await.map_err(multipart_error)? {
        upload.write_chunk(&chunk).await?;
    }
    Ok(upload)
}

/// Read a text part whole, bounded by [`MAX_TEXT_PART_BYTES`].
async fn read_text_part(mut field: Field<'_>, part: &str) -> Result<String, ApiError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = field.chunk().await.map_err(multipart_error)? {
        if bytes.len() + chunk.len() > MAX_TEXT_PART_BYTES {
            return Err(ApiError::BadRequest(format!("'{part}' is too long")));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|_| ApiError::BadRequest(format!("'{part}' is not valid UTF-8")))
}

fn set_once(slot: &mut Option<String>, part: &str, value: String) -> Result<(), ApiError> {
    if slot.is_some() {
        return Err(ApiError::BadRequest(format!("duplicate '{part}' part")));
    }
    *slot = Some(value);
    Ok(())
}

/// 413 when the body limit tripped mid-stream (a body sent without a
/// `Content-Length`); any other multipart failure is a malformed or truncated body.
fn multipart_error(err: MultipartError) -> ApiError {
    if err.status() == StatusCode::PAYLOAD_TOO_LARGE {
        ApiError::PayloadTooLarge(err.body_text())
    } else {
        ApiError::BadRequest(format!("malformed multipart body: {}", err.body_text()))
    }
}

/// `dir` is a normalized relative path (`""` = the entry root).
fn join_relative(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_relative_handles_the_entry_root() {
        assert_eq!(join_relative("", "a.png"), "a.png");
        assert_eq!(join_relative("sub/deeper", "a copy.png"), "sub/deeper/a copy.png");
    }

    #[test]
    fn set_once_rejects_a_repeated_part() {
        let mut slot = None;
        set_once(&mut slot, "pe_id", "pe-1".to_owned()).unwrap();
        let err = set_once(&mut slot, "pe_id", "pe-2".to_owned()).unwrap_err();
        assert_eq!(err.status_code(), StatusCode::BAD_REQUEST);
        assert_eq!(err.public_message(), "duplicate 'pe_id' part");
        assert_eq!(slot.as_deref(), Some("pe-1"), "the first value is kept");
    }
}
