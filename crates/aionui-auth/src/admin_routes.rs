#![allow(clippy::disallowed_types)]

//! Super-admin user management: `/api/admin/users*`.
//!
//! Mounted inside the authenticated route group, so `auth_middleware` has
//! already inserted [`RealUser`]. Every handler authorizes on the *real*
//! caller via [`require_super_admin`], and refuses requests that carry the
//! act-as header: admin operations always run as the admin itself.

use axum::extract::rejection::JsonRejection;
use axum::extract::{Json, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Extension, Router};

use aionui_api_types::{AdminCreateUserRequest, AdminResetPasswordRequest, AdminUserView, ApiResponse};
use aionui_common::ApiError;
use aionui_common::constants::is_super_admin;
use aionui_db::{IUserRepository, UserStatus, models::User};

use crate::account::{AccountError, PasswordOutcome, create_local_user};
use crate::middleware::{ACT_AS_HEADER, RealUser};
use crate::password::hash_password;
use crate::routes::AuthRouterState;
use crate::validation::validate_password;

/// Gate for `/api/admin/*`: the real caller must be the super admin, and the
/// request must not be acting as anyone else.
pub fn require_super_admin(real: &RealUser, headers: &HeaderMap) -> Result<(), ApiError> {
    if headers.contains_key(ACT_AS_HEADER) {
        return Err(ApiError::BadRequest("Admin endpoints do not accept act-as".into()));
    }
    if !is_super_admin(&real.0.id) {
        return Err(ApiError::Forbidden("Super admin required".into()));
    }
    Ok(())
}

pub(crate) fn admin_user_routes() -> Router<AuthRouterState> {
    Router::new()
        .route("/api/admin/users", get(list_users).post(create_user))
        .route("/api/admin/users/{id}", axum::routing::delete(delete_user))
        .route("/api/admin/users/{id}/password", post(reset_password))
        .route("/api/admin/users/{id}/disable", post(disable_user))
        .route("/api/admin/users/{id}/enable", post(enable_user))
}

fn to_view(user: User) -> AdminUserView {
    let from_feishu = user
        .external_user_id
        .as_deref()
        .is_some_and(|id| id.starts_with(crate::feishu::FEISHU_EXTERNAL_PREFIX));
    AdminUserView {
        is_super_admin: is_super_admin(&user.id),
        source: if from_feishu { "feishu" } else { "password" }.to_owned(),
        id: user.id,
        username: user.username.unwrap_or_default(),
        status: user.status.as_str().to_owned(),
        created_at: user.created_at,
        last_login: user.last_login,
        email: user.email,
        avatar_url: user
            .avatar_path
            .filter(|path| path.starts_with("https://") || path.starts_with("http://")),
    }
}

fn db_error(e: impl std::fmt::Display) -> ApiError {
    tracing::error!(error = %e, "admin user operation failed");
    ApiError::Internal("Database error".into())
}

/// Load a live (non-deleted) target user, or 404.
async fn live_target(repo: &dyn IUserRepository, id: &str) -> Result<User, ApiError> {
    repo.find_by_id(id)
        .await
        .map_err(db_error)?
        .filter(|user| user.deleted_at.is_none())
        .ok_or_else(|| ApiError::NotFound(format!("User '{id}' not found")))
}

fn reject_super_admin_target(id: &str, action: &str) -> Result<(), ApiError> {
    if is_super_admin(id) {
        return Err(ApiError::BadRequest(format!("Cannot {action} the super admin")));
    }
    Ok(())
}

fn revoke_sessions(state: &AuthRouterState, user_id: &str) {
    if let Some(hook) = &state.session_revoked_hook {
        hook(user_id);
    }
}

async fn list_users(
    State(state): State<AuthRouterState>,
    Extension(real): Extension<RealUser>,
    headers: HeaderMap,
) -> Result<Json<ApiResponse<Vec<AdminUserView>>>, ApiError> {
    require_super_admin(&real, &headers)?;
    let mut users = state.user_repo.list_users().await.map_err(db_error)?;
    // Super admin first, then by creation time.
    users.sort_by_key(|user| (!is_super_admin(&user.id), user.created_at));
    Ok(Json(ApiResponse::ok(users.into_iter().map(to_view).collect())))
}

async fn create_user(
    State(state): State<AuthRouterState>,
    Extension(real): Extension<RealUser>,
    headers: HeaderMap,
    body: Result<Json<AdminCreateUserRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<AdminUserView>>), ApiError> {
    require_super_admin(&real, &headers)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let outcome = create_local_user(state.user_repo.as_ref(), req.username.trim(), &req.password)
        .await
        .map_err(|e| match e {
            AccountError::InvalidUsername(msg) | AccountError::WeakPassword(msg) => ApiError::BadRequest(msg),
            AccountError::AlreadyExists(name) => ApiError::Conflict(format!("User '{name}' already exists")),
            other => db_error(other),
        })?;
    let PasswordOutcome::Created { id, .. } = outcome else {
        return Err(ApiError::Internal("Unexpected account outcome".into()));
    };
    let user = live_target(state.user_repo.as_ref(), &id).await?;
    tracing::info!(admin_user_id = %real.0.id, user_id = %id, "admin: user created");
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(to_view(user)))))
}

async fn reset_password(
    State(state): State<AuthRouterState>,
    Extension(real): Extension<RealUser>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<AdminResetPasswordRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    require_super_admin(&real, &headers)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let user = live_target(state.user_repo.as_ref(), &id).await?;
    validate_password(&req.password)?;
    let plaintext = req.password;
    let hash = tokio::task::spawn_blocking(move || hash_password(&plaintext))
        .await
        .map_err(|_| ApiError::Internal("Failed to hash password".into()))??;
    state
        .user_repo
        .update_password(&user.id, &hash)
        .await
        .map_err(db_error)?;
    state
        .user_repo
        .increment_session_generation(&user.id)
        .await
        .map_err(db_error)?;
    revoke_sessions(&state, &user.id);
    tracing::info!(admin_user_id = %real.0.id, user_id = %user.id, "admin: password reset");
    Ok(Json(ApiResponse::message("Password reset")))
}

async fn disable_user(
    State(state): State<AuthRouterState>,
    Extension(real): Extension<RealUser>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<AdminUserView>>, ApiError> {
    require_super_admin(&real, &headers)?;
    reject_super_admin_target(&id, "disable")?;
    let user = live_target(state.user_repo.as_ref(), &id).await?;
    state
        .user_repo
        .set_status(&user.id, UserStatus::Disabled)
        .await
        .map_err(db_error)?;
    revoke_sessions(&state, &user.id);
    tracing::info!(admin_user_id = %real.0.id, user_id = %user.id, "admin: user disabled");
    Ok(Json(ApiResponse::ok(to_view(
        live_target(state.user_repo.as_ref(), &id).await?,
    ))))
}

async fn enable_user(
    State(state): State<AuthRouterState>,
    Extension(real): Extension<RealUser>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<AdminUserView>>, ApiError> {
    require_super_admin(&real, &headers)?;
    let user = live_target(state.user_repo.as_ref(), &id).await?;
    state
        .user_repo
        .set_status(&user.id, UserStatus::Active)
        .await
        .map_err(db_error)?;
    tracing::info!(admin_user_id = %real.0.id, user_id = %user.id, "admin: user enabled");
    Ok(Json(ApiResponse::ok(to_view(
        live_target(state.user_repo.as_ref(), &id).await?,
    ))))
}

async fn delete_user(
    State(state): State<AuthRouterState>,
    Extension(real): Extension<RealUser>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    require_super_admin(&real, &headers)?;
    reject_super_admin_target(&id, "delete")?;
    let user = live_target(state.user_repo.as_ref(), &id).await?;
    state.user_repo.soft_delete(&user.id).await.map_err(db_error)?;
    revoke_sessions(&state, &user.id);
    tracing::info!(admin_user_id = %real.0.id, user_id = %user.id, "admin: user deleted");
    Ok(Json(ApiResponse::message("User deleted")))
}
