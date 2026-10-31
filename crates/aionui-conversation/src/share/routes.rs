#![allow(clippy::disallowed_types)]

use std::{sync::Arc, time::Duration};

use aionui_api_types::{ApiResponse, PublicShareMessagesQuery, ShareResourceQuery, UpdateConversationShareRequest};
use aionui_auth::{CurrentUser, RateLimiter, api_rate_limit_middleware};
use aionui_common::ApiError;
use axum::{
    Extension, Json, Router,
    body::Body,
    extract::{Path, Query, Request, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, header},
    middleware::{Next, from_fn, from_fn_with_state},
    response::{IntoResponse, Response},
    routing::get,
};
use tokio::io::AsyncReadExt;

use super::{service::ShareError, state::ShareRouterState};

/// Mount behind the application's normal user-auth and CSRF layers.
pub fn conversation_share_routes(state: ShareRouterState) -> Router {
    Router::new()
        .route("/api/conversations/{id}/share", get(status).put(set_enabled))
        .with_state(state)
        .layer(from_fn(private_response_headers))
}

/// This tree intentionally exposes only GET methods and its own capability
/// authentication; it must not inherit a logged-in owner's identity.
pub fn public_conversation_share_routes(state: ShareRouterState) -> Router {
    let limiter = Arc::new(RateLimiter::new(240, Duration::from_secs(60)));
    Router::new()
        .route("/api/public/shares/current", get(public_conversation))
        .route("/api/public/shares/current/messages", get(public_messages))
        .route("/api/public/shares/current/artifacts", get(public_artifacts))
        .route("/api/public/shares/current/resources", get(resource))
        .route_layer(from_fn_with_state(limiter, api_rate_limit_middleware))
        .with_state(state)
        .layer(from_fn(private_response_headers))
}

async fn status(
    State(state): State<ShareRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.status(&user.id, &id).await.map_err(api_error)?,
    )))
}

async fn set_enabled(
    State(state): State<ShareRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<UpdateConversationShareRequest>, JsonRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .set_enabled(&user.id, &id, request.enabled)
            .await
            .map_err(api_error)?,
    )))
}

async fn public_conversation(
    State(state): State<ShareRouterState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let token = share_token(&headers)?;
    Ok(Json(ApiResponse::ok(
        state.service.public_conversation(&token).await.map_err(api_error)?,
    )))
}

async fn public_messages(
    State(state): State<ShareRouterState>,
    headers: HeaderMap,
    Query(query): Query<PublicShareMessagesQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let token = share_token(&headers)?;
    Ok(Json(ApiResponse::ok(
        state.service.public_messages(&token, query).await.map_err(api_error)?,
    )))
}

async fn public_artifacts(
    State(state): State<ShareRouterState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let token = share_token(&headers)?;
    Ok(Json(ApiResponse::ok(
        state.service.public_artifacts(&token).await.map_err(api_error)?,
    )))
}

async fn resource(
    State(state): State<ShareRouterState>,
    headers: HeaderMap,
    Query(query): Query<ShareResourceQuery>,
) -> Result<Response, ApiError> {
    let token = share_token(&headers)?;
    let resource = state.service.resource(&token, &query).await.map_err(api_error)?;
    let file = tokio::fs::File::open(&resource.path)
        .await
        .map_err(|_| api_error(ShareError::NotFound))?;
    let length = file
        .metadata()
        .await
        .map_err(|_| api_error(ShareError::NotFound))?
        .len();
    let stream = futures_util::stream::try_unfold(file, |mut file| async move {
        let mut buffer = vec![0u8; 64 * 1024];
        let count = file.read(&mut buffer).await?;
        if count == 0 {
            Ok::<_, std::io::Error>(None)
        } else {
            buffer.truncate(count);
            Ok(Some((buffer, file)))
        }
    });
    let mut response = Response::new(Body::from_stream(stream));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&resource.content_type).map_err(|_| api_error(ShareError::Internal))?,
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    let disposition = if resource.attachment { "attachment" } else { "inline" };
    let filename = encode_filename(&resource.filename);
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "{disposition}; filename=\"download\"; filename*=UTF-8''{filename}"
        ))
        .map_err(|_| api_error(ShareError::Internal))?,
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; sandbox"),
    );
    Ok(response)
}

async fn private_response_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert("x-robots-tag", HeaderValue::from_static("noindex, nofollow, noarchive"));
    response
}

fn encode_filename(filename: &str) -> String {
    filename
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

fn share_token(headers: &HeaderMap) -> Result<String, ApiError> {
    headers
        .get("x-share-token")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .ok_or_else(|| api_error(ShareError::NotFound))
}

fn api_error(error: ShareError) -> ApiError {
    match error {
        ShareError::NotFound => ApiError::NotFound("Shared conversation is unavailable".into()),
        ShareError::BadRequest(message) => ApiError::BadRequest(message.into()),
        error => {
            tracing::error!(error = %error, "conversation share operation failed");
            ApiError::Internal("Share service is unavailable".into())
        }
    }
}
