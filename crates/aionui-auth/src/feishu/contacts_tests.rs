use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use aionui_common::encrypt_string;
use aionui_db::{FeishuLoginConfigRow, IFeishuLoginRepository, SqliteFeishuLoginRepository};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::{FeishuContacts, FeishuSenderNameResolver};

struct Platform {
    name: Mutex<Option<String>>,
    returned_open_id: Mutex<Option<String>>,
    token_calls: AtomicUsize,
    contact_calls: AtomicUsize,
    reject: AtomicBool,
    slow: AtomicBool,
    delay_contacts: AtomicBool,
}

struct Fixture {
    _database: aionui_db::Database,
    service: Arc<FeishuContacts>,
    repo: Arc<SqliteFeishuLoginRepository>,
    platform: Arc<Platform>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn fixture(name: Option<&str>) -> Fixture {
    async fn token(State(platform): State<Arc<Platform>>, Json(body): Json<Value>) -> Json<Value> {
        platform.token_calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(body["app_id"], "app");
        assert_eq!(body["app_secret"], "synthetic-contact-test-secret");
        Json(json!({"code":0,"tenant_access_token":"synthetic-contact-token","expire":7200}))
    }
    async fn contact(
        State(platform): State<Arc<Platform>>,
        Path(open_id): Path<String>,
        Query(query): Query<HashMap<String, String>>,
        headers: HeaderMap,
    ) -> (StatusCode, Json<Value>) {
        platform.contact_calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(query.get("user_id_type").map(String::as_str), Some("open_id"));
        assert_eq!(headers["authorization"], "Bearer synthetic-contact-token");
        if platform.reject.load(Ordering::SeqCst) {
            return (StatusCode::FORBIDDEN, Json(json!({"code":99991672})));
        }
        if platform.slow.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        if platform.delay_contacts.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        let user = platform.name.lock().await.as_ref().map(|name| {
            json!({
                "open_id": platform.returned_open_id.try_lock().unwrap().as_deref().unwrap_or(&open_id),
                "name": name,
                "en_name": ""
            })
        });
        (StatusCode::OK, Json(json!({"code":0,"data":{"user":user}})))
    }
    let platform = Arc::new(Platform {
        name: Mutex::new(name.map(str::to_owned)),
        returned_open_id: Mutex::new(None),
        token_calls: AtomicUsize::new(0),
        contact_calls: AtomicUsize::new(0),
        reject: AtomicBool::new(false),
        slow: AtomicBool::new(false),
        delay_contacts: AtomicBool::new(false),
    });
    let router = Router::new()
        .route("/open-apis/auth/v3/tenant_access_token/internal", post(token))
        .route("/open-apis/contact/v3/users/{open_id}", get(contact))
        .with_state(platform.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let database = aionui_db::init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteFeishuLoginRepository::new(database.pool().clone()));
    repo.save(&FeishuLoginConfigRow {
        enabled: true,
        app_id: "app".into(),
        app_secret_enc: Some(encrypt_string("synthetic-contact-test-secret", &[42; 32]).unwrap()),
        tenant_key: Some("tenant".into()),
        public_base_url: "https://web.example.test".into(),
        api_base: Some(api_base),
        ..Default::default()
    })
    .await
    .unwrap();
    let service = Arc::new(FeishuContacts::new(repo.clone(), [42; 32]).unwrap());
    Fixture {
        _database: database,
        service,
        repo,
        platform,
        task,
    }
}

#[tokio::test]
async fn distinct_senders_sync_concurrently_without_blocking_cached_names() {
    let f = fixture(Some("张三")).await;
    assert_eq!(
        f.service.resolve("app", "tenant", "ou_cached").await.as_deref(),
        Some("张三")
    );
    f.platform.delay_contacts.store(true, Ordering::SeqCst);
    let mut tasks = Vec::new();
    for index in 0..8 {
        let service = f.service.clone();
        tasks.push(tokio::spawn(async move {
            service.resolve("app", "tenant", &format!("ou_member_{index}")).await
        }));
    }
    tokio::task::yield_now().await;
    let cached = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        f.service.resolve("app", "tenant", "ou_cached"),
    )
    .await;
    assert_eq!(cached.unwrap().as_deref(), Some("张三"));
    for task in tasks {
        assert_eq!(task.await.unwrap().as_deref(), Some("张三"));
    }
    assert_eq!(f.platform.token_calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.platform.contact_calls.load(Ordering::SeqCst), 9);
}

#[tokio::test]
async fn missing_name_syncs_sender_contact_and_cached_name_skips_another_sync() {
    let f = fixture(Some("张三")).await;
    assert_eq!(
        f.service.resolve("app", "tenant", "ou_member").await.as_deref(),
        Some("张三")
    );
    assert_eq!(f.platform.token_calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.platform.contact_calls.load(Ordering::SeqCst), 1);
    *f.platform.name.lock().await = Some("不应再次读取".into());
    assert_eq!(
        f.service.resolve("app", "tenant", "ou_member").await.as_deref(),
        Some("张三")
    );
    assert_eq!(f.platform.contact_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn absent_or_blank_contact_name_remains_missing_without_repeated_sync() {
    for name in [None, Some(" \n\t "), Some("ou_member")] {
        let f = fixture(name).await;
        assert_eq!(f.service.resolve("app", "tenant", "ou_member").await, None);
        assert_eq!(f.service.resolve("app", "tenant", "ou_member").await, None);
        assert_eq!(f.platform.contact_calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn contact_failure_does_not_fail_message_name_resolution_or_repeat_requests() {
    let f = fixture(Some("张三")).await;
    f.platform.reject.store(true, Ordering::SeqCst);
    assert_eq!(f.service.resolve("app", "tenant", "ou_member").await, None);
    assert_eq!(f.service.resolve("app", "tenant", "ou_member").await, None);
    assert_eq!(f.platform.contact_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn simultaneous_missing_names_share_one_contact_sync() {
    let f = fixture(Some("张三")).await;
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let service = f.service.clone();
        tasks.push(tokio::spawn(async move {
            service.resolve("app", "tenant", "ou_member").await
        }));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap().as_deref(), Some("张三"));
    }
    assert_eq!(f.platform.token_calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.platform.contact_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn names_and_credentials_are_not_used_for_another_app_or_tenant() {
    let f = fixture(Some("张三")).await;
    assert_eq!(f.service.resolve("other_app", "tenant", "ou_member").await, None);
    assert_eq!(f.service.resolve("app", "other_tenant", "ou_member").await, None);
    assert_eq!(f.platform.token_calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.platform.contact_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        f.service.resolve("app", "tenant", "ou_member").await.as_deref(),
        Some("张三")
    );
    assert_eq!(f.service.resolve("other_app", "tenant", "ou_member").await, None);
    assert_eq!(f.service.resolve("app", "other_tenant", "ou_member").await, None);
    let mut config = f.repo.get().await.unwrap().unwrap();
    config.enabled = false;
    f.repo.save(&config).await.unwrap();
    assert_eq!(f.service.resolve("app", "tenant", "ou_member").await, None);
    assert_eq!(f.platform.contact_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_contact_for_another_open_id_is_not_used_as_the_sender_name() {
    let f = fixture(Some("张三")).await;
    *f.platform.returned_open_id.lock().await = Some("ou_different_member".into());
    assert_eq!(f.service.resolve("app", "tenant", "ou_member").await, None);
}

#[tokio::test]
async fn a_slow_directory_has_a_bounded_wait_and_failure_is_cached() {
    let f = fixture(Some("张三")).await;
    f.platform.slow.store(true, Ordering::SeqCst);
    let started = std::time::Instant::now();
    assert_eq!(f.service.resolve("app", "tenant", "ou_member").await, None);
    assert!(started.elapsed() < std::time::Duration::from_secs(4));
    assert_eq!(f.service.resolve("app", "tenant", "ou_member").await, None);
    assert_eq!(f.platform.contact_calls.load(Ordering::SeqCst), 1);
}
