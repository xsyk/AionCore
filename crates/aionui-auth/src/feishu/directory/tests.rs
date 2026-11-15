use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use aionui_common::encrypt_string;
use aionui_db::{
    FeishuLoginConfigRow, FeishuSignupPolicy, IFeishuLoginRepository, IUserRepository, SqliteFeishuLoginRepository,
    SqliteUserRepository, UserStatus,
};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::HeaderMap,
    routing::{get, post},
};
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify};

use super::service::FeishuDirectorySync;

#[derive(Default)]
struct Platform {
    reject_scope: AtomicBool,
    reject_second_page: AtomicBool,
    pause: AtomicBool,
    entered: Notify,
    release: Notify,
    token_calls: AtomicUsize,
    users: Mutex<HashMap<String, Value>>,
}

struct Fixture {
    _db: aionui_db::Database,
    users: Arc<SqliteUserRepository>,
    config: Arc<SqliteFeishuLoginRepository>,
    service: Arc<FeishuDirectorySync>,
    platform: Arc<Platform>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn fixture(policy: FeishuSignupPolicy) -> Fixture {
    async fn token(State(p): State<Arc<Platform>>, Json(body): Json<Value>) -> Json<Value> {
        assert_eq!(body["app_id"], "directory-test-app");
        assert_eq!(body["app_secret"], "synthetic-directory-secret");
        p.token_calls.fetch_add(1, Ordering::SeqCst);
        Json(json!({"code":0,"tenant_access_token":"synthetic-directory-token","expire":7200}))
    }
    fn auth(headers: &HeaderMap) {
        assert_eq!(headers["authorization"], "Bearer synthetic-directory-token");
    }
    async fn scopes(
        State(p): State<Arc<Platform>>,
        Query(q): Query<HashMap<String, String>>,
        h: HeaderMap,
    ) -> Json<Value> {
        auth(&h);
        assert_eq!(q["user_id_type"], "open_id");
        assert_eq!(q["department_id_type"], "open_department_id");
        if p.pause.load(Ordering::SeqCst) {
            p.entered.notify_one();
            p.release.notified().await;
        }
        if p.reject_scope.load(Ordering::SeqCst) {
            return Json(json!({"code":99991672,"msg":"sensitive-upstream-body","data":{}}));
        }
        if q.get("page_token").is_none() {
            Json(
                json!({"code":0,"data":{"user_ids":["ou_one"],"department_ids":["od_visible"],"group_ids":[],"has_more":true,"page_token":"scope-next"}}),
            )
        } else if p.reject_second_page.load(Ordering::SeqCst) {
            Json(json!({"code":99991672}))
        } else {
            assert_eq!(q["page_token"], "scope-next");
            Json(
                json!({"code":0,"data":{"user_ids":["ou_one","ou_two","ou_missing"],"department_ids":[],"group_ids":[],"has_more":false}}),
            )
        }
    }
    async fn departments(Query(q): Query<HashMap<String, String>>, h: HeaderMap) -> Json<Value> {
        auth(&h);
        assert_eq!(q["fetch_child"], "false");
        assert_eq!(q["department_id_type"], "open_department_id");
        let items = if q["parent_department_id"] == "od_visible" {
            json!([{"open_department_id":"od_child"}])
        } else {
            json!([])
        };
        Json(json!({"code":0,"data":{"items":items,"has_more":false}}))
    }
    async fn members(
        State(p): State<Arc<Platform>>,
        Query(q): Query<HashMap<String, String>>,
        h: HeaderMap,
    ) -> Json<Value> {
        auth(&h);
        assert_eq!(q["user_id_type"], "open_id");
        if q["page_size"].parse::<usize>().unwrap() > 50 {
            return Json(json!({"code":40011,"data":{}}));
        }
        let key = if q["department_id"] == "od_visible" {
            "ou_one"
        } else {
            "ou_child"
        };
        let member = p.users.lock().await[key].clone();
        Json(json!({"code":0,"data":{"items":[member],"has_more":false}}))
    }
    async fn user(
        State(p): State<Arc<Platform>>,
        Path(id): Path<String>,
        Query(q): Query<HashMap<String, String>>,
        h: HeaderMap,
    ) -> Json<Value> {
        auth(&h);
        assert_eq!(q["user_id_type"], "open_id");
        let user = p
            .users
            .lock()
            .await
            .get(&id)
            .cloned()
            .unwrap_or(json!({"open_id":id,"name":"缺少稳定身份"}));
        Json(json!({"code":0,"data":{"user":user}}))
    }
    let platform = Arc::new(Platform::default());
    for (id, union, name) in [
        ("ou_one", "on_one", "同名成员"),
        ("ou_two", "on_two", "同名成员"),
        ("ou_child", "on_child", "子部门成员"),
    ] {
        platform.users.lock().await.insert(
            id.into(),
            json!({"open_id":id,"union_id":union,"name":name,"email":format!("{union}@example.test")}),
        );
    }
    let router = Router::new()
        .route("/open-apis/auth/v3/tenant_access_token/internal", post(token))
        .route("/open-apis/contact/v3/scopes", get(scopes))
        .route("/open-apis/contact/v3/departments", get(departments))
        .route("/open-apis/contact/v3/users/find_by_department", get(members))
        .route("/open-apis/contact/v3/users/{id}", get(user))
        .with_state(platform.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = Arc::new(SqliteFeishuLoginRepository::new(db.pool().clone()));
    config
        .save(&FeishuLoginConfigRow {
            enabled: true,
            app_id: "directory-test-app".into(),
            app_secret_enc: Some(encrypt_string("synthetic-directory-secret", &[42; 32]).unwrap()),
            public_base_url: "https://directory.example.test".into(),
            api_base: Some(base),
            signup_policy: policy,
            ..Default::default()
        })
        .await
        .unwrap();
    let users = Arc::new(SqliteUserRepository::new(db.pool().clone()));
    let http = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let service = Arc::new(FeishuDirectorySync::new(config.clone(), [42; 32], users.clone(), http));
    Fixture {
        _db: db,
        users,
        config,
        service,
        platform,
        server,
    }
}

async fn finish(f: &Fixture) -> aionui_api_types::FeishuDirectorySyncView {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let view = f.service.latest().await.unwrap();
            if view.status != "running" {
                return view;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn directory_open_imports_visible_members_once_as_active() {
    let f = fixture(FeishuSignupPolicy::Open).await;
    f.service.start().await.unwrap();
    let result = finish(&f).await;
    assert_eq!(result.status, "completed");
    assert_eq!(
        (result.scanned, result.created, result.skipped, result.failed),
        (4, 3, 1, 0)
    );
    assert!(!result.partial);
    let imported: Vec<_> = f
        .users
        .list_users()
        .await
        .unwrap()
        .into_iter()
        .filter(|u| u.external_user_id.is_some())
        .collect();
    assert_eq!(imported.len(), 3);
    assert!(
        imported
            .iter()
            .all(|u| u.status == UserStatus::Active && u.last_login.is_none())
    );
    assert_ne!(imported[0].username, imported[1].username);
    f.service.start().await.unwrap();
    let again = finish(&f).await;
    assert_eq!((again.created, again.unchanged, again.skipped), (0, 3, 1));
    assert_eq!(f.users.list_users().await.unwrap().len(), 4);
}

#[tokio::test]
async fn directory_backfills_display_name_without_renaming_or_reenabling() {
    let f = fixture(FeishuSignupPolicy::Open).await;
    let existing = f
        .users
        .create_external_local_user(
            "feishu:on_one",
            "feishu-kept-login",
            "synthetic-hash",
            UserStatus::Disabled,
        )
        .await
        .unwrap();
    f.service.start().await.unwrap();
    let result = finish(&f).await;
    assert_eq!(result.status, "completed");
    let after = f.users.find_by_id(&existing.id).await.unwrap().unwrap();
    assert_eq!(after.username, existing.username);
    assert_eq!(after.status, existing.status);
    assert_eq!(after.deleted_at, existing.deleted_at);
    assert_eq!(after.session_generation, existing.session_generation);
    assert_eq!(after.last_login, existing.last_login);
    assert_eq!(serde_json::to_value(&after).unwrap()["display_name"], "同名成员");
}

#[tokio::test]
async fn directory_fills_name_on_later_sync_and_keeps_it_when_upstream_omits_names() {
    let f = fixture(FeishuSignupPolicy::Open).await;
    {
        let mut members = f.platform.users.lock().await;
        members
            .get_mut("ou_one")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("name");
    }
    f.service.start().await.unwrap();
    assert_eq!(finish(&f).await.created, 3);
    let before = f
        .users
        .find_live_local_by_external_id("feishu:on_one")
        .await
        .unwrap()
        .unwrap();
    assert!(before.username.as_deref().unwrap().starts_with("feishu-"));
    f.platform.users.lock().await.get_mut("ou_one").unwrap()["name"] = json!("  目录新姓名  ");
    f.service.start().await.unwrap();
    let result = finish(&f).await;
    assert_eq!((result.created, result.updated, result.failed), (0, 1, 0));
    let named = f.users.find_by_id(&before.id).await.unwrap().unwrap();
    assert_eq!(named.username, before.username);
    assert_eq!(serde_json::to_value(&named).unwrap()["display_name"], "目录新姓名");
    {
        let mut members = f.platform.users.lock().await;
        members.get_mut("ou_one").unwrap()["name"] = json!(" \t\n ");
    }
    f.service.start().await.unwrap();
    let again = finish(&f).await;
    assert_eq!(
        (again.created, again.updated, again.unchanged, again.failed),
        (0, 0, 3, 0)
    );
    let retained = f.users.find_by_id(&before.id).await.unwrap().unwrap();
    assert_eq!(retained.username, before.username);
    assert_eq!(serde_json::to_value(&retained).unwrap()["display_name"], "目录新姓名");
}

#[tokio::test]
async fn directory_display_name_normalizes_and_uses_english_fallback() {
    let f = fixture(FeishuSignupPolicy::Open).await;
    {
        let mut members = f.platform.users.lock().await;
        let one = members.get_mut("ou_one").unwrap();
        one["name"] = json!(" \u{0} ");
        one["en_name"] = json!("  Example   Member  ");
        members.get_mut("ou_two").unwrap()["name"] = json!("  中文\u{0}   成员  ");
        members.get_mut("ou_child").unwrap()["name"] = json!(" ");
    }
    f.service.start().await.unwrap();
    assert_eq!(finish(&f).await.status, "completed");
    for (identity, expected) in [
        ("feishu:on_one", Some("Example Member")),
        ("feishu:on_two", Some("中文 成员")),
        ("feishu:on_child", None),
    ] {
        let account = f.users.find_live_local_by_external_id(identity).await.unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(&account).unwrap()["display_name"].as_str(),
            expected
        );
    }
}

#[tokio::test]
async fn directory_approval_imports_disabled_without_last_login() {
    let f = fixture(FeishuSignupPolicy::Approval).await;
    f.service.start().await.unwrap();
    assert_eq!(finish(&f).await.created, 3);
    for user in f
        .users
        .list_users()
        .await
        .unwrap()
        .into_iter()
        .filter(|u| u.external_user_id.is_some())
    {
        assert_eq!(user.status, UserStatus::Disabled);
        assert_eq!(user.last_login, None);
    }
}

#[tokio::test]
async fn directory_preserves_existing_status_username_and_soft_deletion() {
    let f = fixture(FeishuSignupPolicy::Open).await;
    let disabled = f
        .users
        .create_external_local_user("feishu:on_one", "保留登录名", "synthetic-hash", UserStatus::Disabled)
        .await
        .unwrap();
    let deleted = f
        .users
        .create_external_local_user("feishu:on_two", "已删除成员", "synthetic-hash", UserStatus::Active)
        .await
        .unwrap();
    f.users.soft_delete(&deleted.id).await.unwrap();
    f.service.start().await.unwrap();
    let result = finish(&f).await;
    assert_eq!((result.created, result.updated, result.skipped), (1, 1, 2));
    let unchanged_status = f.users.find_by_id(&disabled.id).await.unwrap().unwrap();
    assert_eq!(unchanged_status.status, UserStatus::Disabled);
    assert_eq!(unchanged_status.username.as_deref(), Some("保留登录名"));
    assert!(
        f.users
            .find_live_local_by_external_id("feishu:on_two")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        f.users
            .find_by_external_user_id(aionui_db::UserType::Local, "feishu:on_two")
            .await
            .unwrap()
            .unwrap()
            .deleted_at
            .is_some()
    );
}

#[tokio::test]
async fn directory_permission_denial_is_fixed_code_without_private_response() {
    let f = fixture(FeishuSignupPolicy::Open).await;
    f.platform.reject_scope.store(true, Ordering::SeqCst);
    f.service.start().await.unwrap();
    let result = finish(&f).await;
    assert_eq!(result.status, "failed");
    assert_eq!(result.error_code.as_deref(), Some("FEISHU_DIRECTORY_PERMISSION_DENIED"));
    assert_eq!(result.created, 0);
    let output = serde_json::to_string(&result).unwrap();
    assert!(!output.contains("sensitive-upstream-body"));
    assert!(!output.contains("synthetic-directory"));
}

#[tokio::test]
async fn directory_partial_pagination_failure_keeps_successful_imports() {
    let f = fixture(FeishuSignupPolicy::Open).await;
    f.platform.reject_second_page.store(true, Ordering::SeqCst);
    f.service.start().await.unwrap();
    let result = finish(&f).await;
    assert!(result.partial);
    assert_eq!(result.status, "failed");
    assert_eq!(result.error_code.as_deref(), Some("FEISHU_DIRECTORY_PERMISSION_DENIED"));
    assert_eq!(result.created, 2);
    assert!(result.failed >= 1);
}

#[tokio::test]
async fn directory_double_start_coalesces_and_config_change_prevents_writes() {
    let f = fixture(FeishuSignupPolicy::Open).await;
    f.platform.pause.store(true, Ordering::SeqCst);
    let first = f.service.start().await.unwrap();
    f.platform.entered.notified().await;
    let second = f.service.start().await.unwrap();
    assert_eq!(first.id, second.id);
    assert_eq!(f.platform.token_calls.load(Ordering::SeqCst), 1);
    let mut row = f.config.get().await.unwrap().unwrap();
    row.signup_policy = FeishuSignupPolicy::Approval;
    f.config.save(&row).await.unwrap();
    f.platform.release.notify_one();
    let result = finish(&f).await;
    assert_eq!(result.error_code.as_deref(), Some("FEISHU_DIRECTORY_CONFIG_CHANGED"));
    assert_eq!(result.created, 0);
}
