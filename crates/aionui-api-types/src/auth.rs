use serde::{Deserialize, Serialize};

/// Public user info returned in API responses.
///
/// Contains only the fields safe to expose to clients.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublicUser {
    pub id: String,
    pub username: String,
}

/// Login request body for `POST /login`.
#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

/// Login success response for `POST /login` and `POST /api/auth/qr-login`.
#[derive(Debug, Serialize, Deserialize)]
pub struct LoginResponse {
    pub success: bool,
    pub message: String,
    pub user: PublicUser,
    pub token: String,
}

impl LoginResponse {
    pub fn new(user: PublicUser, token: String) -> Self {
        Self {
            success: true,
            message: "Login successful".to_owned(),
            user,
            token,
        }
    }
}

/// Change password request body for `POST /api/auth/change-password`.
#[derive(Debug, Deserialize)]
pub struct ChangePasswordRequest {
    pub current_password: String,
    pub new_password: String,
}

/// QR code login request body for `POST /api/auth/qr-login`.
#[derive(Debug, Deserialize)]
pub struct QrLoginRequest {
    pub qr_token: String,
}

/// Auth status response for `GET /api/auth/status`.
#[derive(Debug, Serialize, Deserialize)]
pub struct AuthStatusResponse {
    pub success: bool,
    pub needs_setup: bool,
    pub user_count: u64,
    pub is_authenticated: bool,
}

/// Refresh token request body for `POST /api/auth/refresh`.
#[derive(Debug, Deserialize)]
pub struct RefreshTokenRequest {
    pub token: String,
}

/// The signed-in user as returned by `GET /api/auth/user`.
///
/// `is_super_admin` is computed from the real caller, so it stays `true` while
/// the super admin acts as another user.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CurrentUserInfo {
    pub id: String,
    pub username: String,
    pub is_super_admin: bool,
}

/// User info response for `GET /api/auth/user`.
#[derive(Debug, Serialize)]
pub struct UserInfoResponse {
    pub success: bool,
    pub user: CurrentUserInfo,
}

// ---------------------------------------------------------------------------
// Super-admin user management (`/api/admin/*`)
// ---------------------------------------------------------------------------

/// One account in `GET /api/admin/users`. Never carries secrets.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdminUserView {
    pub id: String,
    pub username: String,
    /// `active` or `disabled`.
    pub status: String,
    pub created_at: aionui_common::TimestampMs,
    pub last_login: Option<aionui_common::TimestampMs>,
    pub is_super_admin: bool,
    /// How the account signs in: `password` or `feishu`.
    pub source: String,
    /// Contact email (synced from Feishu when available); tells namesakes apart.
    pub email: Option<String>,
    /// Feishu avatar; `None` unless the stored value is an http(s) URL.
    pub avatar_url: Option<String>,
}

/// `GET /api/auth/feishu/status` — whether the login page shows the Feishu
/// button, and the site URL the login has to start from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FeishuLoginStatus {
    pub enabled: bool,
    /// Configured site URL while enabled, `None` otherwise. A login page served
    /// from another address starts the flow there.
    pub public_base_url: Option<String>,
}

/// `GET/PUT /api/admin/feishu-login` response. Never carries the app secret.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FeishuLoginConfigView {
    pub enabled: bool,
    pub app_id: String,
    pub app_secret_set: bool,
    pub tenant_key: Option<String>,
    pub public_base_url: String,
    pub api_base: Option<String>,
    pub accounts_base: Option<String>,
    /// `approval` (new Feishu users wait for a super admin) or `open`.
    pub signup_policy: String,
    /// `<public_base_url>/api/auth/feishu/callback`, empty while the base URL is unset.
    pub callback_url: String,
}

/// Body of `PUT /api/admin/feishu-login`. An absent/empty `app_secret` keeps the stored one.
#[derive(Debug, Clone, Deserialize)]
pub struct FeishuLoginConfigUpdate {
    pub enabled: bool,
    pub app_id: String,
    #[serde(default)]
    pub app_secret: Option<String>,
    pub public_base_url: String,
    #[serde(default)]
    pub api_base: Option<String>,
    #[serde(default)]
    pub accounts_base: Option<String>,
    /// `approval` or `open`; absent keeps the stored policy.
    #[serde(default)]
    pub signup_policy: Option<String>,
    #[serde(default)]
    pub clear_tenant_key: bool,
}

/// Body of `POST /api/admin/users`.
#[derive(Debug, Deserialize)]
pub struct AdminCreateUserRequest {
    pub username: String,
    pub password: String,
}

/// Body of `POST /api/admin/users/{id}/password`.
#[derive(Debug, Deserialize)]
pub struct AdminResetPasswordRequest {
    pub password: String,
}

/// Owner of a conversation in the super-admin listing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdminConversationOwner {
    pub id: String,
    pub username: String,
    pub deleted: bool,
}

/// One conversation in `GET /api/admin/conversations`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdminConversationView {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub r#type: String,
    /// `extra.backend` when present (e.g. `claude`, `codex`), for the sider icon.
    pub backend: Option<String>,
    pub updated_at: aionui_common::TimestampMs,
    pub owner: AdminConversationOwner,
}

/// Refresh token response for `POST /api/auth/refresh`.
///
/// `token` is the new access token; `refresh_token` is the rotated refresh
/// token. Native clients persist both and renew via the request body; browsers
/// ignore `refresh_token` and rely on the `Set-Cookie`d refresh cookie instead.
#[derive(Debug, Serialize)]
pub struct RefreshResponse {
    pub success: bool,
    pub token: String,
    pub refresh_token: String,
}

/// WebSocket token response for `GET /api/ws-token`.
#[derive(Debug, Serialize)]
pub struct WsTokenResponse {
    pub success: bool,
    pub ws_token: String,
    pub expires_in: u64,
}

// ---------------------------------------------------------------------------
// Internal AionPro user provisioning endpoints
// ---------------------------------------------------------------------------

/// Request body for `PUT /api/auth/internal/external-users/{external_user_id}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnsureExternalUserRequest {
    pub user_type: ExternalUserType,
    pub username: Option<String>,
    pub email: Option<String>,
    pub avatar_path: Option<String>,
}

/// Supported external identity projection sources.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExternalUserType {
    Aionpro,
}

/// Response for successful internal external-user provisioning.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnsureExternalUserResponse {
    pub user_id: String,
    pub user_type: ExternalUserType,
    pub external_user_id: String,
    pub session_generation: i64,
}

/// Request body for `POST /api/auth/internal/external-sessions`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnsureExternalSessionRequest {
    pub user_type: ExternalUserType,
    pub external_user_id: String,
}

/// Response for successful internal external-session exchange.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnsureExternalSessionResponse {
    pub user: PublicUser,
    pub session_generation: i64,
}

/// Request body for `POST /api/auth/internal/external-sessions/revoke`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RevokeExternalSessionRequest {
    pub user_type: ExternalUserType,
    pub external_user_id: String,
}

/// Response for successful internal external-session revocation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RevokeExternalSessionResponse {
    pub user_id: String,
    pub session_generation: i64,
}

/// Stable internal auth error codes for AionPro/Core session exchange.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum InternalAuthErrorCode {
    BootstrapSecretRequired,
    InvalidBootstrapSecret,
    UserContextRequired,
    UserDisabled,
    ExternalUserConflict,
}

// ---------------------------------------------------------------------------
// WebUI admin credential endpoints (local-only)
// ---------------------------------------------------------------------------

/// Change password request body for `POST /api/webui/change-password`.
///
/// No current_password field — this endpoint is local-mode only and assumes
/// the caller is the trusted Electron main process.
#[derive(Debug, Deserialize)]
pub struct WebuiChangePasswordRequest {
    pub new_password: String,
}

/// Change username request body for `POST /api/webui/change-username`.
#[derive(Debug, Deserialize)]
pub struct WebuiChangeUsernameRequest {
    pub new_username: String,
}

/// Response for `POST /api/webui/change-username`.
#[derive(Debug, Serialize, Deserialize)]
pub struct WebuiChangeUsernameResponse {
    pub username: String,
}

/// Response for `POST /api/webui/reset-password`.
///
/// Returns the freshly generated plaintext password. This is the only time
/// the caller sees it — subsequent reads hit the bcrypt hash only.
#[derive(Debug, Serialize, Deserialize)]
pub struct WebuiResetPasswordResponse {
    pub new_password: String,
}

/// Response for `POST /api/webui/generate-qr-token`.
///
/// Only the token and expiry are returned. URL assembly (host + port) is the
/// caller's responsibility, since only the Electron main process knows which
/// lanIP/port the WebUI is exposed on.
#[derive(Debug, Serialize, Deserialize)]
pub struct WebuiGenerateQrTokenResponse {
    pub token: String,
    pub expires_at_ms: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_public_user_serialization() {
        let user = PublicUser {
            id: "auth_1712345678_abc".into(),
            username: "admin".into(),
        };
        let json = serde_json::to_value(&user).unwrap();
        assert_eq!(json["id"], "auth_1712345678_abc");
        assert_eq!(json["username"], "admin");
    }

    #[test]
    fn test_login_request_deserialization() {
        let raw = r#"{"username":"admin","password":"secret123"}"#;
        let req: LoginRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.username, "admin");
        assert_eq!(req.password, "secret123");
    }

    #[test]
    fn test_login_request_missing_field() {
        let raw = r#"{"username":"admin"}"#;
        let result = serde_json::from_str::<LoginRequest>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn test_login_response_new() {
        let user = PublicUser {
            id: "user_1".into(),
            username: "admin".into(),
        };
        let resp = LoginResponse::new(user.clone(), "jwt_token".into());
        assert!(resp.success);
        assert_eq!(resp.message, "Login successful");
        assert_eq!(resp.user, user);
        assert_eq!(resp.token, "jwt_token");
    }

    #[test]
    fn test_login_response_serialization() {
        let resp = LoginResponse::new(
            PublicUser {
                id: "auth_123".into(),
                username: "admin".into(),
            },
            "eyJhbGciOi".into(),
        );
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["message"], "Login successful");
        assert_eq!(json["user"]["id"], "auth_123");
        assert_eq!(json["user"]["username"], "admin");
        assert_eq!(json["token"], "eyJhbGciOi");
    }

    #[test]
    fn test_internal_external_user_contract_serialization() {
        let req = EnsureExternalUserRequest {
            user_type: ExternalUserType::Aionpro,
            username: Some("Pro User".into()),
            email: Some("pro@example.com".into()),
            avatar_path: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["user_type"], "aionpro");
        assert_eq!(json["username"], "Pro User");
        assert_eq!(json["email"], "pro@example.com");
        assert_eq!(json["avatar_path"], serde_json::Value::Null);

        let resp = EnsureExternalUserResponse {
            user_id: "user_123".into(),
            user_type: ExternalUserType::Aionpro,
            external_user_id: "external_123".into(),
            session_generation: 2,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["user_id"], "user_123");
        assert_eq!(json["user_type"], "aionpro");
        assert_eq!(json["external_user_id"], "external_123");
        assert_eq!(json["session_generation"], 2);

        let code = serde_json::to_value(InternalAuthErrorCode::UserContextRequired).unwrap();
        assert_eq!(code, "USER_CONTEXT_REQUIRED");
    }

    #[test]
    fn test_internal_external_session_revoke_contract_serialization() {
        let req = RevokeExternalSessionRequest {
            user_type: ExternalUserType::Aionpro,
            external_user_id: "external_123".into(),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["user_type"], "aionpro");
        assert_eq!(json["external_user_id"], "external_123");

        let resp = RevokeExternalSessionResponse {
            user_id: "user_123".into(),
            session_generation: 3,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["user_id"], "user_123");
        assert_eq!(json["session_generation"], 3);
    }

    #[test]
    fn test_change_password_request_snake_case() {
        let raw = r#"{"current_password":"old123","new_password":"new456"}"#;
        let req: ChangePasswordRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.current_password, "old123");
        assert_eq!(req.new_password, "new456");
    }

    #[test]
    fn test_change_password_request_camel_case_rejected() {
        let raw = r#"{"currentPassword":"old","newPassword":"new"}"#;
        let result = serde_json::from_str::<ChangePasswordRequest>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn test_qr_login_request_snake_case() {
        let raw = r#"{"qr_token":"abc123"}"#;
        let req: QrLoginRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.qr_token, "abc123");
    }

    #[test]
    fn test_qr_login_request_camel_case_rejected() {
        let raw = r#"{"qrToken":"abc"}"#;
        let result = serde_json::from_str::<QrLoginRequest>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn test_auth_status_response_snake_case() {
        let resp = AuthStatusResponse {
            success: true,
            needs_setup: true,
            user_count: 0,
            is_authenticated: false,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["needs_setup"], true);
        assert_eq!(json["user_count"], 0);
        assert_eq!(json["is_authenticated"], false);
        // Verify snake_case keys exist, not camelCase
        assert!(json.get("needsSetup").is_none());
        assert!(json.get("userCount").is_none());
        assert!(json.get("isAuthenticated").is_none());
    }

    #[test]
    fn test_auth_status_response_deserialization() {
        let raw = json!({
            "success": true,
            "needs_setup": false,
            "user_count": 3,
            "is_authenticated": true
        });
        let resp: AuthStatusResponse = serde_json::from_value(raw).unwrap();
        assert!(resp.success);
        assert!(!resp.needs_setup);
        assert_eq!(resp.user_count, 3);
        assert!(resp.is_authenticated);
    }

    #[test]
    fn test_refresh_token_request_deserialization() {
        let raw = r#"{"token":"eyJhbGciOiJIUzI1NiJ9"}"#;
        let req: RefreshTokenRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.token, "eyJhbGciOiJIUzI1NiJ9");
    }

    #[test]
    fn test_refresh_token_request_missing_token() {
        let raw = r#"{}"#;
        let result = serde_json::from_str::<RefreshTokenRequest>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn test_refresh_response_carries_both_tokens() {
        // Native clients migrating to the dual-token model must be able to read
        // the rotated refresh token out of the JSON body.
        let resp = RefreshResponse {
            success: true,
            token: "access.jwt".into(),
            refresh_token: "refresh.jwt".into(),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["token"], "access.jwt");
        assert_eq!(json["refresh_token"], "refresh.jwt");
    }
}
