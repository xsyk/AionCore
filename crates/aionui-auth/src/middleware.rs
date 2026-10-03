#![allow(clippy::disallowed_types)]

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;

use aionui_common::ApiError;
use aionui_db::{IUserRepository, UserStatus, UserType};

use crate::JwtService;
use crate::extract::extract_token_from_headers;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthIdentityMode {
    Local,
    UserSession,
    AionPro,
}

/// Header carrying the conversation-runtime helper token minted by the backend
/// and injected into agent subprocess environments as `AIONUI_RUNTIME_TOKEN`.
pub const RUNTIME_TOKEN_HEADER: &str = "x-aionui-runtime-token";
/// Header carrying the acting user id asserted by the helper CLI.
pub const RUNTIME_USER_ID_HEADER: &str = "x-aionui-user-id";
/// Header carrying the conversation id the helper CLI runs inside.
pub const RUNTIME_CONVERSATION_ID_HEADER: &str = "x-aionui-conversation-id";
/// Header a super admin sends to run a request as another user (the owner of
/// the conversation being operated on). Any other caller sending it is refused.
pub const ACT_AS_HEADER: &str = "x-aionui-act-as";

/// Port for validating conversation-runtime helper tokens.
///
/// Implemented in the composition layer over the agent runtime's token
/// service; `aionui-auth` must not depend on `aionui-ai-agent` directly.
/// A verifier must confirm the token is a live, conversation-helper-scoped
/// token bound to exactly this (user_id, conversation_id) pair.
pub trait IRuntimeTokenVerifier: Send + Sync {
    fn verify_conversation_helper(&self, token: &str, user_id: &str, conversation_id: &str) -> bool;
}

/// Authenticated user injected into request extensions by the auth middleware.
///
/// Route handlers extract this from `request.extensions()` to identify
/// the current user.
#[derive(Debug, Clone)]
pub struct CurrentUser {
    /// User ID from the database.
    pub id: String,
    /// Username.
    pub username: String,
    /// Internal identity source for the current user.
    pub user_type: UserType,
    /// Current account status. Authenticated requests only receive active users.
    pub status: UserStatus,
}

impl CurrentUser {
    pub fn local_default() -> Self {
        Self {
            id: "system_default_user".to_string(),
            username: "system_default_user".to_string(),
            user_type: UserType::Local,
            status: UserStatus::Active,
        }
    }
}

/// The authenticated caller, before any act-as substitution.
///
/// Always inserted next to [`CurrentUser`]. The two differ only while the super
/// admin acts as another user; admin-only routes authorize on this, never on
/// `CurrentUser`.
#[derive(Debug, Clone)]
pub struct RealUser(pub CurrentUser);

/// Shared state for the authentication middleware.
#[derive(Clone)]
pub struct AuthState {
    pub jwt_service: Arc<JwtService>,
    pub user_repo: Arc<dyn IUserRepository>,
    pub identity_mode: AuthIdentityMode,
    /// Optional second credential channel for agent-subprocess helper CLIs
    /// (`aioncore config` / `diagnose`), which cannot carry a JWT or cookies.
    /// `None` disables the channel (requests without a JWT are rejected).
    pub runtime_token_verifier: Option<Arc<dyn IRuntimeTokenVerifier>>,
}

/// Authentication middleware that verifies JWT tokens and injects `CurrentUser`.
///
/// Flow:
/// 1. Extract bearer token from `Authorization` header or `aionui-session` cookie
/// 2. Verify JWT signature, expiration, and blacklist
/// 3. Look up user in the database to ensure they still exist
/// 4. Insert [`CurrentUser`] into request extensions
///
/// Returns HTTP 401 for authentication failures.
///
/// Use with `axum::middleware::from_fn_with_state`.
pub async fn auth_middleware(
    State(state): State<AuthState>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    // In local mode, skip JWT verification and inject a fixed default user.
    if state.identity_mode == AuthIdentityMode::Local {
        request.extensions_mut().insert(RealUser(CurrentUser::local_default()));
        request.extensions_mut().insert(CurrentUser::local_default());
        return Ok(next.run(request).await);
    }

    let Some(token) = extract_token_from_headers(request.headers()) else {
        // No JWT/cookie: fall back to the conversation-helper runtime-token
        // channel used by agent subprocess CLIs.
        return runtime_token_channel(&state, request, next).await;
    };

    // Require an *access* token here: a refresh token must never authenticate an
    // ordinary request — it is accepted only at the refresh endpoint.
    let payload = state.jwt_service.verify_access(&token).map_err(|e| {
        tracing::debug!("Token verification failed: {e}");
        ApiError::Unauthorized("Invalid or expired token".into())
    })?;

    let user = state
        .user_repo
        .find_active_by_id(&payload.user_id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "auth middleware user lookup failed");
            ApiError::Internal("Authentication service unavailable".into())
        })?
        .ok_or_else(|| ApiError::Unauthorized("Invalid authentication subject".into()))?;

    if state.identity_mode == AuthIdentityMode::AionPro && user.user_type != UserType::Aionpro {
        return Err(ApiError::coded(
            StatusCode::UNAUTHORIZED,
            "USER_CONTEXT_REQUIRED",
            "User context required.",
            None,
        ));
    }

    if payload.session_generation != user.session_generation {
        return Err(ApiError::Unauthorized("Invalid authentication session".into()));
    }

    let real = CurrentUser {
        id: user.id,
        username: user.username.unwrap_or_else(|| "external_user".to_string()),
        user_type: user.user_type,
        status: user.status,
    };
    apply_act_as(&state, &mut request, real).await?;

    Ok(next.run(request).await)
}

/// Insert [`RealUser`] and the effective [`CurrentUser`] for a JWT-authenticated
/// request, honouring [`ACT_AS_HEADER`].
///
/// Only the super admin may act as someone else, and only as an existing,
/// active, non-deleted user other than itself. Downstream handlers keep reading
/// `CurrentUser`, so they run with the target's identity and configuration.
async fn apply_act_as(state: &AuthState, request: &mut Request, real: CurrentUser) -> Result<(), ApiError> {
    let target = request
        .headers()
        .get(ACT_AS_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);

    let acting = match target {
        None => real.clone(),
        Some(target_id) => {
            if !aionui_common::constants::is_super_admin(&real.id) {
                return Err(ApiError::Forbidden(
                    "Acting as another user requires the super admin".into(),
                ));
            }
            if target_id == real.id {
                return Err(ApiError::BadRequest("Cannot act as yourself".into()));
            }
            let user = state
                .user_repo
                .find_active_by_id(&target_id)
                .await
                .map_err(|e| {
                    tracing::error!(error = %e, "act-as user lookup failed");
                    ApiError::Internal("Authentication service unavailable".into())
                })?
                .ok_or_else(|| ApiError::NotFound("Act-as target user not found".into()))?;
            tracing::info!(
                real_user_id = %real.id,
                acting_user_id = %user.id,
                method = %request.method(),
                path = %request.uri().path(),
                "auth: act-as"
            );
            CurrentUser {
                id: user.id,
                username: user.username.unwrap_or_else(|| "external_user".to_string()),
                user_type: user.user_type,
                status: user.status,
            }
        }
    };

    request.extensions_mut().insert(RealUser(real));
    request.extensions_mut().insert(acting);
    Ok(())
}

/// Authenticate a JWT-less request via the conversation-helper runtime token.
///
/// The helper CLI sends the token the backend minted for its conversation
/// runtime plus the (user, conversation) pair the token was bound to. The
/// verifier enforces that binding, so a forged user or conversation header
/// fails closed. On success the token's user is loaded and injected as
/// [`CurrentUser`], making ordinary user-scoped handlers work unchanged.
async fn runtime_token_channel(state: &AuthState, mut request: Request, next: Next) -> Result<Response, ApiError> {
    let headers = request.headers();
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let (Some(verifier), Some(token), Some(user_id), Some(conversation_id)) = (
        state.runtime_token_verifier.as_ref(),
        header(RUNTIME_TOKEN_HEADER),
        header(RUNTIME_USER_ID_HEADER),
        header(RUNTIME_CONVERSATION_ID_HEADER),
    ) else {
        return Err(ApiError::Unauthorized("Authentication required".into()));
    };

    if !verifier.verify_conversation_helper(&token, &user_id, &conversation_id) {
        return Err(ApiError::Unauthorized("Invalid runtime token".into()));
    }

    let user = state
        .user_repo
        .find_active_by_id(&user_id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "runtime token channel user lookup failed");
            ApiError::Internal("Authentication service unavailable".into())
        })?
        .ok_or_else(|| ApiError::Unauthorized("Invalid authentication subject".into()))?;

    if state.identity_mode == AuthIdentityMode::AionPro && user.user_type != UserType::Aionpro {
        return Err(ApiError::coded(
            StatusCode::UNAUTHORIZED,
            "USER_CONTEXT_REQUIRED",
            "User context required.",
            None,
        ));
    }

    let current = CurrentUser {
        id: user.id,
        username: user.username.unwrap_or_else(|| "external_user".to_string()),
        user_type: user.user_type,
        status: user.status,
    };
    // The helper channel never acts as anyone else.
    request.extensions_mut().insert(RealUser(current.clone()));
    request.extensions_mut().insert(current);

    Ok(next.run(request).await)
}

/// Local-mode authentication middleware that skips JWT verification.
///
/// Injects a fixed `CurrentUser` with id and username `system_default_user`.
/// Used when the server runs as an embedded subprocess inside Electron.
pub async fn local_auth_middleware(mut request: Request, next: Next) -> Response {
    request.extensions_mut().insert(RealUser(CurrentUser::local_default()));
    request.extensions_mut().insert(CurrentUser::local_default());
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt;

    async fn echo_user(request: Request<Body>) -> String {
        let user = request.extensions().get::<CurrentUser>().unwrap();
        format!("{}:{}", user.id, user.username)
    }

    // ── act-as ────────────────────────────────────────────────────────

    async fn echo_current_and_real(request: Request<Body>) -> String {
        let current = request.extensions().get::<CurrentUser>().unwrap();
        let real = request.extensions().get::<RealUser>().unwrap();
        format!("{}|{}", current.id, real.0.id)
    }

    struct ActAsFixture {
        app: Router,
        jwt: Arc<JwtService>,
        repo: Arc<dyn IUserRepository>,
        _db: aionui_db::Database,
    }

    async fn act_as_fixture() -> ActAsFixture {
        let db = aionui_db::init_database_memory().await.unwrap();
        let repo: Arc<dyn IUserRepository> = Arc::new(aionui_db::SqliteUserRepository::new(db.pool().clone()));
        let jwt = Arc::new(JwtService::new("act-as-test-secret".to_string()));
        let state = AuthState {
            jwt_service: jwt.clone(),
            user_repo: repo.clone(),
            identity_mode: AuthIdentityMode::UserSession,
            runtime_token_verifier: None,
        };
        let app = Router::new()
            .route("/test", get(echo_current_and_real))
            .route_layer(axum::middleware::from_fn_with_state(state, auth_middleware));
        ActAsFixture {
            app,
            jwt,
            repo,
            _db: db,
        }
    }

    async fn token_for(f: &ActAsFixture, user_id: &str) -> String {
        let user = f.repo.find_by_id(user_id).await.unwrap().unwrap();
        f.jwt
            .sign_with_session_generation(
                &user.id,
                user.username.as_deref().unwrap_or("u"),
                user.session_generation,
            )
            .unwrap()
    }

    async fn call(f: &ActAsFixture, token: &str, act_as: Option<&str>) -> (StatusCode, String) {
        let mut builder = Request::builder()
            .uri("/test")
            .header("authorization", format!("Bearer {token}"));
        if let Some(target) = act_as {
            builder = builder.header(ACT_AS_HEADER, target);
        }
        let response = f
            .app
            .clone()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    const ADMIN: &str = "system_default_user";

    #[tokio::test]
    async fn act_as_absent_keeps_caller_identity() {
        let f = act_as_fixture().await;
        let token = token_for(&f, ADMIN).await;
        assert_eq!(
            call(&f, &token, None).await,
            (StatusCode::OK, format!("{ADMIN}|{ADMIN}"))
        );
    }

    #[tokio::test]
    async fn super_admin_acts_as_active_user() {
        let f = act_as_fixture().await;
        let alice = f.repo.create_user("alice", "h").await.unwrap();
        let token = token_for(&f, ADMIN).await;
        assert_eq!(
            call(&f, &token, Some(&alice.id)).await,
            (StatusCode::OK, format!("{}|{ADMIN}", alice.id))
        );
    }

    #[tokio::test]
    async fn non_admin_act_as_is_forbidden() {
        let f = act_as_fixture().await;
        let alice = f.repo.create_user("alice", "h").await.unwrap();
        let bob = f.repo.create_user("bob", "h").await.unwrap();
        let token = token_for(&f, &alice.id).await;
        assert_eq!(call(&f, &token, Some(&bob.id)).await.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn super_admin_cannot_act_as_itself() {
        let f = act_as_fixture().await;
        let token = token_for(&f, ADMIN).await;
        assert_eq!(call(&f, &token, Some(ADMIN)).await.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn act_as_missing_disabled_or_deleted_target_is_not_found() {
        let f = act_as_fixture().await;
        let token = token_for(&f, ADMIN).await;
        assert_eq!(call(&f, &token, Some("ghost")).await.0, StatusCode::NOT_FOUND);

        let carol = f.repo.create_user("carol", "h").await.unwrap();
        f.repo.set_status(&carol.id, UserStatus::Disabled).await.unwrap();
        assert_eq!(call(&f, &token, Some(&carol.id)).await.0, StatusCode::NOT_FOUND);

        let dave = f.repo.create_user("dave", "h").await.unwrap();
        f.repo.soft_delete(&dave.id).await.unwrap();
        assert_eq!(call(&f, &token, Some(&dave.id)).await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_local_auth_middleware_injects_default_user() {
        let app = Router::new()
            .route("/test", get(echo_user))
            .route_layer(axum::middleware::from_fn(local_auth_middleware));

        let response = app
            .oneshot(Request::builder().uri("/test").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            std::str::from_utf8(&body).unwrap(),
            "system_default_user:system_default_user"
        );
    }
}
