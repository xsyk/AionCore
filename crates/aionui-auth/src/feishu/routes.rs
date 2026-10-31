#![allow(clippy::disallowed_types)]

//! Feishu OAuth endpoints and the super-admin config endpoints.
//!
//! Every login failure redirects to `/#/login?feishu_error=<code>` instead of
//! returning JSON, because the browser arrives here by top-level navigation.

use axum::extract::rejection::JsonRejection;
use axum::extract::{Json, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Router};
use serde::Deserialize;

use aionui_api_types::{
    ApiResponse, FeishuInAppLoginRequest, FeishuInAppLoginResponse, FeishuLoginConfigUpdate, FeishuLoginConfigView,
    FeishuLoginStatus, PublicUser,
};
use aionui_common::ApiError;
use aionui_common::constants::{CSRF_COOKIE_NAME, CSRF_HEADER_NAME};

use super::{FEISHU_STATE_COOKIE, FeishuLogin, FeishuLoginError, account, pkce};
use crate::admin_routes::require_super_admin;
use crate::extract::extract_cookie_value;
use crate::middleware::RealUser;
use crate::routes::{AuthRouterState, constant_time_eq, issue_session_cookies};

const STATE_COOKIE_PATH: &str = "/api/auth/feishu";
const STATE_MAX_AGE_SECS: u32 = 600;

pub(crate) fn feishu_public_routes() -> Router<AuthRouterState> {
    Router::new()
        .route("/api/auth/feishu/status", get(status))
        .route("/api/auth/feishu/start", get(start))
        .route("/api/auth/feishu/callback", get(callback))
        .route("/api/auth/feishu/in-app", post(in_app))
}

pub(crate) fn feishu_admin_routes() -> Router<AuthRouterState> {
    Router::new().route("/api/admin/feishu-login", get(get_config).put(put_config))
}

/// Feishu login is only meaningful for real per-user sessions.
fn service(state: &AuthRouterState) -> Option<&FeishuLogin> {
    if state.local || state.aionpro_mode {
        return None;
    }
    state.feishu.as_deref()
}

/// Must be `SameSite=Lax`: Feishu sends the browser back with a cross-site
/// top-level GET, which drops `Strict` cookies.
fn state_cookie(state: &AuthRouterState, value: &str, max_age: u32) -> String {
    format!(
        "{FEISHU_STATE_COOKIE}={value}; Path={STATE_COOKIE_PATH}; HttpOnly; SameSite=Lax{}; Max-Age={max_age}",
        if state.cookie_config.secure { "; Secure" } else { "" },
    )
}

fn redirect(location: &str, cookies: &[String]) -> Response {
    let mut resp = (StatusCode::FOUND, [(header::LOCATION, location.to_owned())]).into_response();
    for cookie in cookies {
        if let Ok(value) = HeaderValue::from_str(cookie) {
            resp.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    resp
}

fn login_error(state: &AuthRouterState, err: &FeishuLoginError) -> Response {
    match err {
        FeishuLoginError::Upstream(_) | FeishuLoginError::Server(_) | FeishuLoginError::Invalid(_) => {
            tracing::warn!(error = %err, "feishu login failed");
        }
        _ => tracing::info!(reason = err.code(), "feishu login rejected"),
    }
    redirect(
        &format!("/#/login?feishu_error={}", err.code()),
        &[state_cookie(state, "", 0)],
    )
}

async fn status(State(state): State<AuthRouterState>, headers: HeaderMap) -> Response {
    let config = match service(&state) {
        Some(svc) => svc.resolved().await.ok(),
        None => None,
    };
    let mut response = Json(ApiResponse::ok(FeishuLoginStatus {
        enabled: config.is_some(),
        public_base_url: config.as_ref().map(|cfg| cfg.public_base_url.clone()),
        app_id: config.as_ref().map(|cfg| cfg.app_id.clone()),
    }))
    .into_response();
    // Some existing Web deployments disable the global CSRF layer. This new
    // authentication endpoint still requires a same-origin double-submit token.
    if extract_cookie_value(&headers, CSRF_COOKIE_NAME).is_none_or(|token| token.is_empty()) {
        let cookie = state
            .cookie_config
            .build_csrf_cookie(&crate::csrf::generate_csrf_token());
        if let Ok(value) = HeaderValue::from_str(&cookie) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn in_app_error(error: FeishuLoginError) -> ApiError {
    tracing::info!(reason = error.code(), "feishu in-client login rejected");
    let (status, code) = match error {
        FeishuLoginError::Disabled => (StatusCode::SERVICE_UNAVAILABLE, "FEISHU_LOGIN_DISABLED"),
        FeishuLoginError::Tenant => (StatusCode::FORBIDDEN, "FEISHU_LOGIN_TENANT"),
        FeishuLoginError::AccountDisabled => (StatusCode::FORBIDDEN, "FEISHU_LOGIN_ACCOUNT_DISABLED"),
        FeishuLoginError::PendingApproval => (StatusCode::FORBIDDEN, "FEISHU_LOGIN_PENDING"),
        FeishuLoginError::Cancelled | FeishuLoginError::State => (StatusCode::BAD_REQUEST, "FEISHU_LOGIN_CODE"),
        FeishuLoginError::Upstream(_) => (StatusCode::BAD_GATEWAY, "FEISHU_LOGIN_UPSTREAM"),
        FeishuLoginError::Server(_) | FeishuLoginError::Invalid(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, "FEISHU_LOGIN_SERVER")
        }
    };
    ApiError::coded(status, code, "Feishu login could not be completed", None)
}

async fn in_app(
    State(state): State<AuthRouterState>,
    headers: HeaderMap,
    body: Result<Json<FeishuInAppLoginRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let cookie = extract_cookie_value(&headers, CSRF_COOKIE_NAME).unwrap_or_default();
    let csrf = headers
        .get(CSRF_HEADER_NAME)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if cookie.is_empty() || !constant_time_eq(cookie.as_bytes(), csrf.as_bytes()) {
        return Err(ApiError::CsrfInvalid("CSRF token validation failed".into()));
    }
    let svc = service(&state).ok_or_else(|| in_app_error(FeishuLoginError::Disabled))?;
    let cfg = svc.resolved().await.map_err(in_app_error)?;
    let expected_origin = reqwest::Url::parse(&cfg.public_base_url)
        .map_err(|_| in_app_error(FeishuLoginError::Invalid("Invalid site URL".into())))?
        .origin()
        .ascii_serialization();
    if headers.get(header::ORIGIN).and_then(|value| value.to_str().ok()) != Some(expected_origin.as_str()) {
        return Err(ApiError::coded(
            StatusCode::FORBIDDEN,
            "FEISHU_LOGIN_ORIGIN",
            "Invalid login origin",
            None,
        ));
    }
    if cfg.tenant_key.as_deref().is_none_or(|tenant| tenant.is_empty()) {
        return Err(ApiError::coded(
            StatusCode::FORBIDDEN,
            "FEISHU_LOGIN_TENANT_UNCONFIGURED",
            "Configure the allowed enterprise before using in-client login",
            None,
        ));
    }
    let Json(request) = body.map_err(ApiError::from)?;
    let code = request.code.trim();
    if code.is_empty() || code.len() > 4096 {
        return Err(ApiError::coded(
            StatusCode::BAD_REQUEST,
            "FEISHU_LOGIN_CODE",
            "Invalid authorization code",
            None,
        ));
    }
    let identity = svc.exchange_in_app_and_fetch(&cfg, code).await.map_err(in_app_error)?;
    svc.check_tenant(&cfg, &identity.tenant_key)
        .await
        .map_err(in_app_error)?;
    let user = account::resolve_account(state.user_repo.as_ref(), &identity, cfg.signup_policy)
        .await
        .map_err(in_app_error)?;
    let (_, cookies) = issue_session_cookies(&state, &user)
        .map_err(|error| in_app_error(FeishuLoginError::Server(error.to_string())))?;
    if state.user_repo.update_last_login(&user.id).await.is_err() {
        tracing::warn!(user_id=%user.id,"feishu in-client last-login update failed");
    }
    tracing::info!(user_id=%user.id,"feishu in-client login succeeded");
    let mut response = Json(ApiResponse::ok(FeishuInAppLoginResponse {
        user: PublicUser {
            id: user.id,
            username: user.username.unwrap_or_default(),
        },
    }))
    .into_response();
    for cookie in cookies {
        response.headers_mut().append(
            header::SET_COOKIE,
            HeaderValue::from_str(&cookie).map_err(|_| ApiError::Internal("Invalid session cookie".into()))?,
        );
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

async fn start(State(state): State<AuthRouterState>) -> Response {
    let Some(svc) = service(&state) else {
        return login_error(&state, &FeishuLoginError::Disabled);
    };
    let cfg = match svc.resolved().await {
        Ok(cfg) => cfg,
        Err(err) => return login_error(&state, &err),
    };
    let (Some(nonce), Some(verifier)) = (pkce::random_token(), pkce::random_token()) else {
        return login_error(&state, &FeishuLoginError::Server("entropy unavailable".into()));
    };
    redirect(
        &FeishuLogin::authorize_url(&cfg, &nonce, &pkce::challenge_s256(&verifier)),
        &[state_cookie(
            &state,
            &pkce::encode_state_cookie(&nonce, &verifier),
            STATE_MAX_AGE_SECS,
        )],
    )
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
}

async fn callback(
    State(state): State<AuthRouterState>,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> Response {
    match finish_login(&state, &headers, q).await {
        Ok(cookies) => redirect("/#/guid", &cookies),
        Err(err) => login_error(&state, &err),
    }
}

async fn finish_login(
    state: &AuthRouterState,
    headers: &HeaderMap,
    q: CallbackQuery,
) -> Result<Vec<String>, FeishuLoginError> {
    let svc = service(state).ok_or(FeishuLoginError::Disabled)?;
    let cfg = svc.resolved().await?;
    let raw = extract_cookie_value(headers, FEISHU_STATE_COOKIE).unwrap_or_default();
    let (expected, verifier) = pkce::decode_state_cookie(&raw).ok_or(FeishuLoginError::State)?;
    let got = q.state.unwrap_or_default();
    if !constant_time_eq(expected.as_bytes(), got.as_bytes()) {
        return Err(FeishuLoginError::State);
    }
    let code = q.code.filter(|c| !c.is_empty()).ok_or(FeishuLoginError::Cancelled)?;
    let feishu_user = svc.exchange_and_fetch(&cfg, &code, verifier).await?;
    svc.check_tenant(&cfg, &feishu_user.tenant_key).await?;
    let user = account::resolve_account(state.user_repo.as_ref(), &feishu_user, cfg.signup_policy).await?;
    let (_, [session, refresh]) =
        issue_session_cookies(state, &user).map_err(|e| FeishuLoginError::Server(e.to_string()))?;
    // Best effort: a missing last_login only makes a later disable read as
    // "pending" instead of "account_disabled"; it never blocks this login.
    if let Err(e) = state.user_repo.update_last_login(&user.id).await {
        tracing::warn!(user_id = %user.id, error = %e, "feishu: failed to update last login");
    }
    tracing::info!(user_id = %user.id, "feishu login succeeded");
    Ok(vec![session, refresh, state_cookie(state, "", 0)])
}

fn config_error(err: FeishuLoginError) -> ApiError {
    match err {
        FeishuLoginError::Invalid(msg) => ApiError::BadRequest(msg),
        other => {
            tracing::error!(error = %other, "feishu config operation failed");
            ApiError::Internal("Feishu config error".into())
        }
    }
}

fn admin_service(state: &AuthRouterState) -> Result<&FeishuLogin, ApiError> {
    service(state).ok_or_else(|| ApiError::BadRequest("Feishu login is unavailable in this mode".into()))
}

async fn get_config(
    State(state): State<AuthRouterState>,
    Extension(real): Extension<RealUser>,
    headers: HeaderMap,
) -> Result<Json<ApiResponse<FeishuLoginConfigView>>, ApiError> {
    require_super_admin(&real, &headers)?;
    let view = admin_service(&state)?.view().await.map_err(config_error)?;
    Ok(Json(ApiResponse::ok(view)))
}

async fn put_config(
    State(state): State<AuthRouterState>,
    Extension(real): Extension<RealUser>,
    headers: HeaderMap,
    body: Result<Json<FeishuLoginConfigUpdate>, JsonRejection>,
) -> Result<Json<ApiResponse<FeishuLoginConfigView>>, ApiError> {
    require_super_admin(&real, &headers)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let view = admin_service(&state)?.update(req).await.map_err(config_error)?;
    tracing::info!(
        admin_user_id = %real.0.id,
        enabled = view.enabled,
        signup_policy = %view.signup_policy,
        "admin: feishu login config updated"
    );
    Ok(Json(ApiResponse::ok(view)))
}
