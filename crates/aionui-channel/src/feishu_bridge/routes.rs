#![allow(clippy::disallowed_types)]

use std::sync::Arc;

use aionui_api_types::{ApiResponse, FeishuBindingCode, FeishuBindingStatus};
use aionui_auth::{CurrentUser, RealUser};
use aionui_common::ApiError;
use axum::extract::{Extension, Path, State};
use axum::routing::get;
use axum::{Json, Router};

use super::FeishuBridge;

/// The composition layer applies ordinary authentication and CSRF middleware.
pub fn feishu_binding_routes(bridge: Arc<FeishuBridge>) -> Router {
    Router::new()
        .route(
            "/api/conversations/{id}/feishu-binding",
            get(status).post(create_code).delete(unbind),
        )
        .with_state(bridge)
}

fn caller(real: &RealUser, effective: &CurrentUser) -> Result<String, ApiError> {
    if real.0.id != effective.id {
        return Err(ApiError::Forbidden(
            "Only the conversation owner may manage its Feishu binding".into(),
        ));
    }
    Ok(real.0.id.clone())
}

async fn status(
    State(bridge): State<Arc<FeishuBridge>>,
    Extension(real): Extension<RealUser>,
    Extension(effective): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<FeishuBindingStatus>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        bridge.status(&caller(&real, &effective)?, &id).await?,
    )))
}

async fn create_code(
    State(bridge): State<Arc<FeishuBridge>>,
    Extension(real): Extension<RealUser>,
    Extension(effective): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<FeishuBindingCode>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        bridge.create_binding_code(&caller(&real, &effective)?, &id).await?,
    )))
}

async fn unbind(
    State(bridge): State<Arc<FeishuBridge>>,
    Extension(real): Extension<RealUser>,
    Extension(effective): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    bridge.unbind(&caller(&real, &effective)?, &id).await?;
    Ok(Json(ApiResponse::success()))
}
