use std::sync::Arc;

use aionui_api_types::FeishuLoginConfigUpdate;
use aionui_db::{IUserRepository, SqliteFeishuLoginRepository, SqliteUserRepository, UserStatus};

use super::account::{resolve_account, truncate_bytes, username_candidates};
use super::client::FeishuUser;
use super::{FeishuLogin, FeishuLoginError};

fn user(union_id: &str, name: &str) -> FeishuUser {
    FeishuUser {
        union_id: union_id.into(),
        name: name.into(),
        en_name: String::new(),
        avatar_url: None,
        email: None,
        tenant_key: "t1".into(),
    }
}

async fn repos() -> (Arc<SqliteUserRepository>, FeishuLogin, aionui_db::Database) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let users = Arc::new(SqliteUserRepository::new(db.pool().clone()));
    let svc = FeishuLogin::new(
        Arc::new(SqliteFeishuLoginRepository::new(db.pool().clone())),
        [7u8; 32],
        reqwest::Client::new(),
    );
    (users, svc, db)
}

fn update(enabled: bool, secret: Option<&str>) -> FeishuLoginConfigUpdate {
    FeishuLoginConfigUpdate {
        enabled,
        app_id: "cli_a".into(),
        app_secret: secret.map(str::to_owned),
        public_base_url: "https://aidi.example.com/".into(),
        api_base: None,
        accounts_base: None,
        clear_tenant_key: false,
    }
}

#[test]
fn truncate_bytes_respects_char_boundary() {
    assert_eq!(truncate_bytes("abc", 32), "abc");
    assert_eq!(truncate_bytes("张三李四", 7), "张三"); // 3 bytes per char
    assert_eq!(truncate_bytes(&"a".repeat(40), 32).len(), 32);
}

#[test]
fn username_candidates_fallbacks() {
    let u = user("on_0123456789abcdef", "张三");
    assert_eq!(
        username_candidates(&u),
        vec!["张三", "张三-cdef", "张三-89abcdef", "feishu-456789abcdef"]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
    );
    let blank = FeishuUser {
        name: "  ".into(),
        en_name: "Zhang".into(),
        ..user("on_xyz", "")
    };
    assert_eq!(username_candidates(&blank)[0], "Zhang");
    let none = user("on_abcdef123456", "");
    assert_eq!(username_candidates(&none)[0], "feishu-123456");
    for c in username_candidates(&user(&"u".repeat(40), &"名".repeat(20))) {
        assert!(c.len() <= 32, "{c} too long");
    }
}

#[tokio::test]
async fn resolve_account_creates_reuses_and_rejects() {
    let (users, _svc, _db) = repos().await;
    let a = resolve_account(users.as_ref(), &user("on_1111", "张三")).await.unwrap();
    assert_eq!(a.username.as_deref(), Some("张三"));
    assert_eq!(a.external_user_id.as_deref(), Some("feishu:on_1111"));
    // same person again -> same account
    let again = resolve_account(users.as_ref(), &user("on_1111", "张三")).await.unwrap();
    assert_eq!(again.id, a.id);
    // namesake -> suffixed username
    let b = resolve_account(users.as_ref(), &user("on_2222", "张三")).await.unwrap();
    assert_eq!(b.username.as_deref(), Some("张三-2222"));
    // disabled -> rejected
    users.set_status(&a.id, UserStatus::Disabled).await.unwrap();
    let err = resolve_account(users.as_ref(), &user("on_1111", "张三"))
        .await
        .unwrap_err();
    assert!(matches!(err, FeishuLoginError::AccountDisabled));
    // soft-deleted -> fresh account
    users.soft_delete(&a.id).await.unwrap();
    let c = resolve_account(users.as_ref(), &user("on_1111", "张三")).await.unwrap();
    assert_ne!(c.id, a.id);
    assert_eq!(c.status, UserStatus::Active);
}

#[tokio::test]
async fn resolve_account_skips_taken_email() {
    let (users, _svc, _db) = repos().await;
    let mut first = user("on_a", "A");
    first.email = Some("same@corp.com".into());
    let a = resolve_account(users.as_ref(), &first).await.unwrap();
    assert_eq!(a.email.as_deref(), Some("same@corp.com"));
    let mut second = user("on_b", "B");
    second.email = Some("same@corp.com".into());
    let b = resolve_account(users.as_ref(), &second).await.unwrap();
    assert!(b.email.is_none(), "taken email must not block login");
}

#[tokio::test]
async fn config_secret_is_encrypted_and_kept_when_blank() {
    let (_users, svc, _db) = repos().await;
    let err = svc.update(update(true, None)).await.unwrap_err();
    assert!(matches!(err, FeishuLoginError::Invalid(_)), "enabling needs a secret");
    let view = svc.update(update(true, Some("s3cret"))).await.unwrap();
    assert!(view.app_secret_set);
    assert_eq!(view.public_base_url, "https://aidi.example.com");
    assert_eq!(view.callback_url, "https://aidi.example.com/api/auth/feishu/callback");
    // blank secret keeps the stored one
    svc.update(update(true, Some(""))).await.unwrap();
    let cfg = svc.resolved().await.unwrap();
    assert_eq!(cfg.app_secret, "s3cret");
    assert_eq!(cfg.api_base, "https://open.feishu.cn");
    assert_eq!(cfg.accounts_base, "https://accounts.feishu.cn");
    assert_eq!(
        FeishuLogin::authorize_url(&cfg, "st"),
        "https://accounts.feishu.cn/open-apis/authen/v1/authorize?client_id=cli_a&response_type=code&redirect_uri=https%3A%2F%2Faidi.example.com%2Fapi%2Fauth%2Ffeishu%2Fcallback&state=st"
    );
}

#[tokio::test]
async fn config_rejects_bad_urls_and_disabled_resolves_to_disabled() {
    let (_users, svc, _db) = repos().await;
    assert!(matches!(svc.resolved().await.unwrap_err(), FeishuLoginError::Disabled));
    let mut bad = update(true, Some("s"));
    bad.public_base_url = "aidi.example.com".into();
    assert!(matches!(
        svc.update(bad).await.unwrap_err(),
        FeishuLoginError::Invalid(_)
    ));
    svc.update(update(false, None)).await.unwrap(); // disabled may be incomplete
    assert!(matches!(svc.resolved().await.unwrap_err(), FeishuLoginError::Disabled));
}

#[tokio::test]
async fn tenant_locks_on_first_login() {
    let (_users, svc, _db) = repos().await;
    svc.update(update(true, Some("s"))).await.unwrap();
    let cfg = svc.resolved().await.unwrap();
    svc.check_tenant(&cfg, "t1").await.unwrap();
    assert_eq!(svc.view().await.unwrap().tenant_key.as_deref(), Some("t1"));
    let cfg = svc.resolved().await.unwrap();
    assert!(matches!(
        svc.check_tenant(&cfg, "t2").await.unwrap_err(),
        FeishuLoginError::Tenant
    ));
    let mut clear = update(true, None);
    clear.clear_tenant_key = true;
    assert!(svc.update(clear).await.unwrap().tenant_key.is_none());
}
