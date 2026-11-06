//! Real auth routes and repositories with a deterministic node identity backend.
//! Protocol signing is separately checked against captured actual Go HTTP fixtures.
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use aionui_api_types::{
    FeishuGatewayIdentity, FeishuGatewayIdentityBeginRequest, FeishuGatewayIdentityClaims,
    FeishuGatewayIdentityExchange, FeishuGatewayIdentityExchangeRequest, FeishuGatewayIdentityTransaction,
    FeishuIdentityKind,
};
use aionui_auth::feishu::{
    FeishuLoginError,
    gateway_identity::{FeishuGatewayIdentityBackend, GatewayIdentityTrust},
    gateway_login::GatewayIdentityLogin,
};
use aionui_auth::{
    AuthIdentityMode, AuthRouterState, CookieConfig, FeishuLogin, JwtService, QrTokenStore, RefreshCoalescer,
    auth_routes,
};
use aionui_common::constants::{COOKIE_NAME, CSRF_COOKIE_NAME};
use aionui_db::{
    FeishuLoginConfigRow, FeishuSignupPolicy, IFeishuLoginRepository, IUserRepository,
    SqliteFeishuIdentityTransactionRepository, SqliteFeishuLoginRepository, SqliteUserRepository, UserStatus,
    init_database_memory,
};
use async_trait::async_trait;
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Request, StatusCode, Uri, header},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signer, SigningKey};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

const BASE: &str = "https://qa.example";
const CALLBACK: &str = "https://qa.example/api/auth/feishu/callback";
const APP: &str = "cli_gateway_test";
const NODE: &str = "node_gateway_test";
const TENANT: &str = "tenant_gateway_test";
const UNION: &str = "union_gateway_test";
const CODE: &str = "platform-code-test-only";
const STATE_COOKIE: &str = "aionui-feishu-state";
const CSRF: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Valid,
    ForeignPin,
    ExternalUrl,
    DuplicateState,
    WrongRedirect,
    BadAssertion,
    ExchangeFailure,
}

struct Backend {
    key: SigningKey,
    mode: Mutex<Mode>,
    nonces: Mutex<HashMap<String, String>>,
    begin_calls: AtomicUsize,
    exchange_calls: AtomicUsize,
}

impl Backend {
    fn new(mode: Mode) -> Self {
        Self {
            key: SigningKey::from_bytes(&[29; 32]),
            mode: Mutex::new(mode),
            nonces: Mutex::new(HashMap::new()),
            begin_calls: AtomicUsize::new(0),
            exchange_calls: AtomicUsize::new(0),
        }
    }
    fn trust(&self) -> GatewayIdentityTrust {
        let public = self.key.verifying_key().to_bytes();
        GatewayIdentityTrust {
            node_id: NODE.into(),
            app_id: APP.into(),
            tenant_key: TENANT.into(),
            verification_key: URL_SAFE_NO_PAD.encode(public),
            key_id: hash(&public),
        }
    }
}

fn hash(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(bytes))
}

#[async_trait]
impl FeishuGatewayIdentityBackend for Backend {
    async fn begin(
        &self,
        request: FeishuGatewayIdentityBeginRequest,
    ) -> Result<FeishuGatewayIdentityTransaction, FeishuLoginError> {
        let number = self.begin_calls.fetch_add(1, Ordering::SeqCst) + 1;
        let transaction_id = format!("{number:043}");
        self.nonces
            .lock()
            .unwrap()
            .insert(transaction_id.clone(), request.nonce);
        let mode = *self.mode.lock().unwrap();
        let mut authorization_url = None;
        if request.kind == FeishuIdentityKind::Oauth {
            let mut url = reqwest::Url::parse("https://accounts.feishu.cn/open-apis/authen/v1/authorize").unwrap();
            url.query_pairs_mut()
                .append_pair("client_id", APP)
                .append_pair("response_type", "code")
                .append_pair(
                    "redirect_uri",
                    if mode == Mode::WrongRedirect {
                        "https://attacker.invalid/callback"
                    } else {
                        &request.redirect_uri
                    },
                )
                .append_pair("state", &transaction_id)
                .append_pair("code_challenge", &"c".repeat(43))
                .append_pair("code_challenge_method", "S256");
            if mode == Mode::DuplicateState {
                url.query_pairs_mut().append_pair("state", "foreign");
            }
            authorization_url = Some(if mode == Mode::ExternalUrl {
                "https://attacker.invalid/authorize".into()
            } else {
                url.into()
            });
        }
        let trust = self.trust();
        Ok(FeishuGatewayIdentityTransaction {
            transaction_id,
            expires_at: aionui_common::now_ms() + 600_000,
            app_id: APP.into(),
            authorization_url,
            verification_key: if mode == Mode::ForeignPin {
                URL_SAFE_NO_PAD.encode(SigningKey::from_bytes(&[30; 32]).verifying_key().to_bytes())
            } else {
                trust.verification_key
            },
            key_id: trust.key_id,
        })
    }
    async fn exchange(
        &self,
        request: FeishuGatewayIdentityExchangeRequest,
    ) -> Result<FeishuGatewayIdentityExchange, FeishuLoginError> {
        self.exchange_calls.fetch_add(1, Ordering::SeqCst);
        let mode = *self.mode.lock().unwrap();
        if mode == Mode::ExchangeFailure {
            return Err(FeishuLoginError::Upstream("test gateway exchange unavailable".into()));
        }
        if request.code != CODE || self.nonces.lock().unwrap().get(&request.transaction_id) != Some(&request.nonce) {
            return Err(FeishuLoginError::State);
        }
        let identity = FeishuGatewayIdentity {
            union_id: UNION.into(),
            open_id: "open_test".into(),
            tenant_key: TENANT.into(),
            name: "Gateway test user".into(),
            email: "gateway-test@example.invalid".into(),
            avatar_url: String::new(),
        };
        let now = aionui_common::now_ms() / 1000;
        let claims = FeishuGatewayIdentityClaims {
            iss: "aion-feishu-route".into(),
            aud: NODE.into(),
            sub: format!("feishu:{UNION}"),
            tenant_key: TENANT.into(),
            iat: now,
            exp: now + 120,
            jti: request.transaction_id,
            nonce_hash: hash(request.nonce.as_bytes()),
            identity: identity.clone(),
        };
        let head = URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&json!({"alg":"EdDSA","typ":"JWT","kid":self.trust().key_id})).unwrap());
        let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
        let unsigned = format!("{head}.{body}");
        let key = if mode == Mode::BadAssertion {
            SigningKey::from_bytes(&[30; 32])
        } else {
            self.key.clone()
        };
        let assertion = format!(
            "{unsigned}.{}",
            URL_SAFE_NO_PAD.encode(key.sign(unsigned.as_bytes()).to_bytes())
        );
        // The unsigned convenience identity must never select the local account.
        let mut projection = identity;
        projection.union_id = "unsigned_attacker".into();
        Ok(FeishuGatewayIdentityExchange {
            identity: projection,
            assertion,
            expires_at: (now + 120) * 1000 + 789,
        })
    }
}

struct Context {
    app: Router,
    backend: Arc<Backend>,
    users: Arc<SqliteUserRepository>,
    direct_calls: Arc<AtomicUsize>,
    trap: tokio::task::JoinHandle<()>,
    _db: aionui_db::Database,
}

impl Drop for Context {
    fn drop(&mut self) {
        self.trap.abort();
    }
}

async fn direct_trap(State(calls): State<Arc<AtomicUsize>>, uri: Uri) -> axum::Json<Value> {
    calls.fetch_add(1, Ordering::SeqCst);
    axum::Json(if uri.path().ends_with("/token") {
        json!({"code":0,"access_token":"direct-platform-token-test-only"})
    } else {
        json!({"code":0,"data":{"union_id":"direct_fallback_must_not_run","name":"Direct fallback","tenant_key":TENANT}})
    })
}

async fn setup(mode: Mode, policy: FeishuSignupPolicy) -> Context {
    let db = init_database_memory().await.unwrap();
    let users = Arc::new(SqliteUserRepository::new(db.pool().clone()));
    let config = Arc::new(SqliteFeishuLoginRepository::new(db.pool().clone()));
    let backend = Arc::new(Backend::new(mode));
    let login = Arc::new(
        GatewayIdentityLogin::new(
            backend.trust(),
            backend.clone(),
            Arc::new(SqliteFeishuIdentityTransactionRepository::new(db.pool().clone())),
        )
        .unwrap(),
    );
    let direct_calls = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let direct_base = format!("http://{}", listener.local_addr().unwrap());
    let trap_app = Router::new().fallback(direct_trap).with_state(direct_calls.clone());
    let trap = tokio::spawn(async move {
        axum::serve(listener, trap_app).await.unwrap();
    });
    config
        .save(&FeishuLoginConfigRow {
            enabled: true,
            app_id: APP.into(),
            app_secret_enc: None,
            tenant_key: Some(TENANT.into()),
            public_base_url: BASE.into(),
            api_base: Some(direct_base),
            accounts_base: None,
            signup_policy: policy,
            updated_at: 0,
        })
        .await
        .unwrap();
    let feishu = FeishuLogin::new(config, [7; 32], reqwest::Client::builder().no_proxy().build().unwrap())
        .with_gateway(Some(login));
    let state = AuthRouterState {
        jwt_service: Arc::new(JwtService::new("gateway-route-test-jwt-secret".into())),
        refresh_coalescer: RefreshCoalescer::new(),
        user_repo: users.clone(),
        fs_adopter: None,
        cookie_config: Arc::new(CookieConfig {
            secure: true,
            same_site: "Strict",
        }),
        qr_token_store: Arc::new(QrTokenStore::new()),
        identity_mode: AuthIdentityMode::UserSession,
        bootstrap_secret: None,
        session_revoked_hook: None,
        local: false,
        aionpro_mode: false,
        feishu: Some(Arc::new(feishu)),
    };
    Context {
        app: auth_routes(state),
        backend,
        users,
        direct_calls,
        trap,
        _db: db,
    }
}

struct Response {
    status: StatusCode,
    location: String,
    cookies: Vec<String>,
    body: Value,
}

async fn call(
    ctx: &Context,
    method: &str,
    uri: &str,
    cookie: Option<&str>,
    origin: Option<&str>,
    csrf: Option<&str>,
    body: Option<Value>,
) -> Response {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    if let Some(origin) = origin {
        request = request.header(header::ORIGIN, origin);
    }
    if let Some(csrf) = csrf {
        request = request.header("x-csrf-token", csrf);
    }
    if body.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    let response = ctx
        .app
        .clone()
        .oneshot(
            request
                .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let location = response
        .headers()
        .get(header::LOCATION)
        .map(|value| value.to_str().unwrap().into())
        .unwrap_or_default();
    let cookies = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap().into())
        .collect();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Response {
        status,
        location,
        cookies,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

fn cookie_pair(response: &Response, name: &str) -> Option<String> {
    response
        .cookies
        .iter()
        .find(|cookie| cookie.starts_with(&format!("{name}=")))
        .map(|value| value.split(';').next().unwrap().into())
}

fn assert_no_session(response: &Response) {
    assert!(cookie_pair(response, COOKIE_NAME).is_none());
}

struct Pending {
    cookie: String,
    transaction: String,
}
async fn start(ctx: &Context) -> Pending {
    let response = call(ctx, "GET", "/api/auth/feishu/start", None, None, None, None).await;
    assert_eq!(response.status, StatusCode::FOUND);
    let url = reqwest::Url::parse(&response.location)
        .expect("valid login must redirect to the pinned official authorize URL");
    assert_eq!(url.host_str(), Some("accounts.feishu.cn"));
    assert_eq!(
        url.query_pairs().find(|(key, _)| key == "redirect_uri").unwrap().1,
        CALLBACK
    );
    let cookie = cookie_pair(&response, STATE_COOKIE).unwrap();
    assert!(
        response
            .cookies
            .iter()
            .any(|value| value.starts_with(&format!("{STATE_COOKIE}="))
                && value.contains("HttpOnly")
                && value.contains("Secure")
                && value.contains("SameSite=Lax"))
    );
    let value = cookie.split_once('=').unwrap().1;
    let parts: Vec<_> = value.split('.').collect();
    assert_eq!(parts.len(), 4);
    assert_eq!(parts[0], "g1");
    assert_eq!(parts[1].len(), 43);
    assert_eq!(parts[2].len(), 43);
    assert_eq!(parts[3].len(), 43);
    let transaction = parts[1].to_owned();
    Pending { cookie, transaction }
}

async fn callback(ctx: &Context, pending: &Pending) -> Response {
    call(
        ctx,
        "GET",
        &format!("/api/auth/feishu/callback?code={CODE}&state={}", pending.transaction),
        Some(&pending.cookie),
        None,
        None,
        None,
    )
    .await
}

async fn h5(ctx: &Context) -> Response {
    call(
        ctx,
        "POST",
        "/api/auth/feishu/in-app",
        Some(&format!("{CSRF_COOKIE_NAME}={CSRF}")),
        Some(BASE),
        Some(CSRF),
        Some(json!({"code":CODE})),
    )
    .await
}

async fn me(ctx: &Context, response: &Response) -> Value {
    let cookie = cookie_pair(response, COOKIE_NAME).expect("successful login issues a normal session cookie");
    let response = call(ctx, "GET", "/api/auth/user", Some(&cookie), None, None, None).await;
    assert_eq!(response.status, StatusCode::OK);
    response.body["user"].clone()
}

#[tokio::test]
async fn oauth_and_h5_without_vm_secret_reuse_the_verified_account() {
    let ctx = setup(Mode::Valid, FeishuSignupPolicy::Open).await;
    let status = call(&ctx, "GET", "/api/auth/feishu/status", None, None, None, None).await;
    assert_eq!(status.body["data"]["enabled"], true);
    let pending = start(&ctx).await;
    let login = callback(&ctx, &pending).await;
    assert_eq!(login.status, StatusCode::FOUND);
    assert_eq!(login.location, "/#/guid");
    let existing = me(&ctx, &login).await;
    let in_app = h5(&ctx).await;
    assert_eq!(in_app.status, StatusCode::OK);
    assert_eq!(me(&ctx, &in_app).await["id"], existing["id"]);
    assert!(
        in_app
            .cookies
            .iter()
            .any(|cookie| cookie.starts_with(&format!("{COOKIE_NAME}="))
                && cookie.contains("HttpOnly")
                && cookie.contains("Secure"))
    );
    assert!(
        ctx.users
            .find_live_local_by_external_id("feishu:unsigned_attacker")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        ctx.users
            .find_live_local_by_external_id(&format!("feishu:{UNION}"))
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(ctx.backend.begin_calls.load(Ordering::SeqCst), 2);
    assert_eq!(ctx.backend.exchange_calls.load(Ordering::SeqCst), 2);
    assert_eq!(ctx.direct_calls.load(Ordering::SeqCst), 0);
    let body = in_app.body.to_string();
    assert!(!body.contains("assertion"));
    assert!(!body.contains("platform-token"));
    assert!(!body.contains(CODE));
}

#[tokio::test]
async fn oauth_browser_binding_and_successful_callback_replay_are_rejected_locally() {
    let ctx = setup(Mode::Valid, FeishuSignupPolicy::Open).await;
    let pending = start(&ctx).await;
    let mut parts: Vec<_> = pending.cookie.split('.').map(str::to_owned).collect();
    parts[3] = "z".repeat(43);
    let rebound = Pending {
        cookie: parts.join("."),
        transaction: pending.transaction.clone(),
    };
    let rejected = callback(&ctx, &rebound).await;
    assert_eq!(rejected.location, "/#/login?feishu_error=state");
    assert_no_session(&rejected);
    assert_eq!(ctx.backend.exchange_calls.load(Ordering::SeqCst), 0);
    let login = callback(&ctx, &pending).await;
    assert_eq!(login.location, "/#/guid");
    let replay = callback(&ctx, &pending).await;
    assert_eq!(replay.location, "/#/login?feishu_error=state");
    assert_no_session(&replay);
    assert_eq!(ctx.backend.exchange_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn h5_csrf_and_origin_fail_before_creating_any_gateway_transaction() {
    let ctx = setup(Mode::Valid, FeishuSignupPolicy::Open).await;
    let missing = call(
        &ctx,
        "POST",
        "/api/auth/feishu/in-app",
        None,
        Some(BASE),
        None,
        Some(json!({"code":CODE})),
    )
    .await;
    assert_eq!(missing.status, StatusCode::FORBIDDEN);
    assert_eq!(missing.body["code"], "CSRF_INVALID");
    let foreign = call(
        &ctx,
        "POST",
        "/api/auth/feishu/in-app",
        Some(&format!("{CSRF_COOKIE_NAME}={CSRF}")),
        Some("https://attacker.invalid"),
        Some(CSRF),
        Some(json!({"code":CODE})),
    )
    .await;
    assert_eq!(foreign.status, StatusCode::FORBIDDEN);
    assert_eq!(foreign.body["code"], "FEISHU_LOGIN_ORIGIN");
    assert_eq!(ctx.backend.begin_calls.load(Ordering::SeqCst), 0);
    assert_eq!(ctx.backend.exchange_calls.load(Ordering::SeqCst), 0);
    assert_no_session(&missing);
    assert_no_session(&foreign);
}

#[tokio::test]
async fn begin_rejects_foreign_pin_and_untrusted_authorization_redirects() {
    for mode in [
        Mode::ForeignPin,
        Mode::ExternalUrl,
        Mode::DuplicateState,
        Mode::WrongRedirect,
    ] {
        let ctx = setup(mode, FeishuSignupPolicy::Open).await;
        let response = call(&ctx, "GET", "/api/auth/feishu/start", None, None, None, None).await;
        assert_eq!(response.status, StatusCode::FOUND);
        assert_eq!(response.location, "/#/login?feishu_error=upstream");
        assert_no_session(&response);
        assert_eq!(ctx.backend.exchange_calls.load(Ordering::SeqCst), 0);
        assert_eq!(ctx.direct_calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn exchange_failure_or_bad_signature_never_falls_back_to_vm_direct_auth() {
    for mode in [Mode::ExchangeFailure, Mode::BadAssertion] {
        let ctx = setup(mode, FeishuSignupPolicy::Open).await;
        let pending = start(&ctx).await;
        let denied = callback(&ctx, &pending).await;
        assert_eq!(denied.location, "/#/login?feishu_error=upstream");
        assert_no_session(&denied);
        let denied_h5 = h5(&ctx).await;
        assert_eq!(denied_h5.status, StatusCode::BAD_GATEWAY);
        assert_eq!(denied_h5.body["code"], "FEISHU_LOGIN_UPSTREAM");
        assert_no_session(&denied_h5);
        assert_eq!(ctx.backend.exchange_calls.load(Ordering::SeqCst), 2);
        assert_eq!(ctx.direct_calls.load(Ordering::SeqCst), 0);
        assert!(
            ctx.users
                .find_live_local_by_external_id(&format!("feishu:{UNION}"))
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn gateway_login_preserves_pending_approval_and_disabled_account_policies() {
    let pending = setup(Mode::Valid, FeishuSignupPolicy::Approval).await;
    let denied = h5(&pending).await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert_eq!(denied.body["code"], "FEISHU_LOGIN_PENDING");
    assert_no_session(&denied);
    let ctx = setup(Mode::Valid, FeishuSignupPolicy::Open).await;
    assert_eq!(h5(&ctx).await.status, StatusCode::OK);
    let account = ctx
        .users
        .find_live_local_by_external_id(&format!("feishu:{UNION}"))
        .await
        .unwrap()
        .unwrap();
    ctx.users.set_status(&account.id, UserStatus::Disabled).await.unwrap();
    let denied = h5(&ctx).await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert_eq!(denied.body["code"], "FEISHU_LOGIN_ACCOUNT_DISABLED");
    assert_no_session(&denied);
    assert_eq!(ctx.direct_calls.load(Ordering::SeqCst), 0);
}
