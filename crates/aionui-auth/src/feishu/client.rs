//! Feishu open-platform HTTP calls used by web login.

use serde::Deserialize;

use super::FeishuLoginError;

/// Identity returned by `authen/v1/user_info`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeishuUser {
    pub union_id: String,
    pub name: String,
    pub en_name: String,
    pub avatar_url: Option<String>,
    /// `enterprise_email`, falling back to `email`.
    pub email: Option<String>,
    pub tenant_key: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    code: i64,
    access_token: Option<String>,
    error_description: Option<String>,
}

#[derive(Deserialize)]
struct UserInfoResponse {
    #[serde(default)]
    code: i64,
    msg: Option<String>,
    data: Option<UserInfoData>,
}

#[derive(Deserialize, Default)]
struct UserInfoData {
    #[serde(default)]
    union_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    en_name: String,
    avatar_url: Option<String>,
    email: Option<String>,
    enterprise_email: Option<String>,
    #[serde(default)]
    tenant_key: String,
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

/// `POST {api_base}/open-apis/authen/v2/oauth/token` → user access token.
pub async fn exchange_code(
    http: &reqwest::Client,
    api_base: &str,
    app_id: &str,
    app_secret: &str,
    code: &str,
    redirect_uri: &str,
) -> Result<String, FeishuLoginError> {
    let resp = http
        .post(format!("{api_base}/open-apis/authen/v2/oauth/token"))
        .json(&serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": app_id,
            "client_secret": app_secret,
            "code": code,
            "redirect_uri": redirect_uri,
        }))
        .send()
        .await
        .map_err(|e| FeishuLoginError::Upstream(format!("token request failed: {e}")))?;
    let status = resp.status();
    let body: TokenResponse = resp
        .json()
        .await
        .map_err(|e| FeishuLoginError::Upstream(format!("token response ({status}) unreadable: {e}")))?;
    match body.access_token {
        Some(token) if body.code == 0 && !token.is_empty() => Ok(token),
        _ => Err(FeishuLoginError::Upstream(format!(
            "token rejected ({status}, code {}): {}",
            body.code,
            body.error_description.unwrap_or_default()
        ))),
    }
}

/// `GET {api_base}/open-apis/authen/v1/user_info`.
pub async fn fetch_user(
    http: &reqwest::Client,
    api_base: &str,
    access_token: &str,
) -> Result<FeishuUser, FeishuLoginError> {
    let resp = http
        .get(format!("{api_base}/open-apis/authen/v1/user_info"))
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|e| FeishuLoginError::Upstream(format!("user_info request failed: {e}")))?;
    let status = resp.status();
    let body: UserInfoResponse = resp
        .json()
        .await
        .map_err(|e| FeishuLoginError::Upstream(format!("user_info response ({status}) unreadable: {e}")))?;
    let data = match body.data {
        Some(data) if body.code == 0 => data,
        _ => {
            return Err(FeishuLoginError::Upstream(format!(
                "user_info rejected ({status}, code {}): {}",
                body.code,
                body.msg.unwrap_or_default()
            )));
        }
    };
    if data.union_id.is_empty() || data.tenant_key.is_empty() {
        return Err(FeishuLoginError::Upstream(
            "user_info missing union_id or tenant_key".into(),
        ));
    }
    Ok(FeishuUser {
        union_id: data.union_id,
        name: data.name,
        en_name: data.en_name,
        avatar_url: non_empty(data.avatar_url),
        email: non_empty(data.enterprise_email).or_else(|| non_empty(data.email)),
        tenant_key: data.tenant_key,
    })
}
