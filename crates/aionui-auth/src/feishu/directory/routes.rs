#![allow(clippy::disallowed_types)]
//! 超级管理员通讯录同步端点，沿用真实身份及双提交CSRF边界。

use aionui_api_types::{ApiResponse, FeishuDirectorySyncRequest, FeishuDirectorySyncView};
use aionui_common::{
    ApiError,
    constants::{CSRF_COOKIE_NAME, CSRF_HEADER_NAME},
};
use axum::{
    Extension, Json, Router,
    extract::{State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    routing::get,
};

use super::service::{DirectoryError, FeishuDirectorySync};
use crate::{
    admin_routes::require_super_admin,
    extract::extract_cookie_value,
    middleware::RealUser,
    routes::{AuthRouterState, constant_time_eq},
};

pub(crate) fn directory_routes() -> Router<AuthRouterState> {
    Router::new().route("/api/admin/feishu-directory/sync", get(latest).post(start))
}

fn api_error(error: DirectoryError) -> ApiError {
    let status = match error {
        DirectoryError::ConfigUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        DirectoryError::PermissionDenied => StatusCode::FORBIDDEN,
        DirectoryError::ConfigChanged => StatusCode::CONFLICT,
        DirectoryError::Timeout => StatusCode::GATEWAY_TIMEOUT,
        DirectoryError::Failed => StatusCode::BAD_GATEWAY,
    };
    ApiError::coded(status, error.code(), "飞书通讯录同步未完成", None)
}

fn service(state: &AuthRouterState) -> Result<&std::sync::Arc<FeishuDirectorySync>, ApiError> {
    if state.local || state.aionpro_mode {
        return Err(api_error(DirectoryError::ConfigUnavailable));
    }
    state
        .feishu_directory
        .as_ref()
        .ok_or_else(|| api_error(DirectoryError::ConfigUnavailable))
}

async fn latest(
    State(state): State<AuthRouterState>,
    Extension(real): Extension<RealUser>,
    headers: HeaderMap,
) -> Result<Json<ApiResponse<Option<FeishuDirectorySyncView>>>, ApiError> {
    require_super_admin(&real, &headers)?;
    Ok(Json(ApiResponse::ok(service(&state)?.latest().await)))
}

async fn start(
    State(state): State<AuthRouterState>,
    Extension(real): Extension<RealUser>,
    headers: HeaderMap,
    body: Result<Json<FeishuDirectorySyncRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<FeishuDirectorySyncView>>, ApiError> {
    require_super_admin(&real, &headers)?;
    let cookie = extract_cookie_value(&headers, CSRF_COOKIE_NAME).unwrap_or_default();
    let header = headers
        .get(CSRF_HEADER_NAME)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if cookie.is_empty() || !constant_time_eq(cookie.as_bytes(), header.as_bytes()) {
        return Err(ApiError::CsrfInvalid("CSRF token validation failed".into()));
    }
    let Json(_) = body.map_err(|_| ApiError::BadRequest("同步请求必须为空对象".into()))?;
    Ok(Json(ApiResponse::ok(
        service(&state)?.start().await.map_err(api_error)?,
    )))
}
