use aionui_db::{
    ConsumeFeishuIdentityTransactionParams, FeishuIdentityTransactionKind, FeishuIdentityTransactionRow,
    IFeishuIdentityTransactionRepository, SqliteFeishuIdentityTransactionRepository, SqlitePool, init_database_memory,
};

const NOW: i64 = 1_790_000_000_000;

fn transaction(fill: char) -> FeishuIdentityTransactionRow {
    FeishuIdentityTransactionRow {
        transaction_id: fill.to_string().repeat(43),
        node_id: "node_214".into(),
        kind: FeishuIdentityTransactionKind::Oauth,
        nonce_hash: "n".repeat(43),
        browser_binding_hash: "b".repeat(43),
        redirect_uri: "https://qa.easiful.com/api/auth/feishu/callback".into(),
        expires_at: NOW + 600_000,
        consumed_at: None,
    }
}

fn admission(row: &FeishuIdentityTransactionRow) -> ConsumeFeishuIdentityTransactionParams {
    ConsumeFeishuIdentityTransactionParams {
        transaction_id: row.transaction_id.clone(),
        node_id: row.node_id.clone(),
        kind: row.kind,
        nonce_hash: row.nonce_hash.clone(),
        browser_binding_hash: row.browser_binding_hash.clone(),
        redirect_uri: row.redirect_uri.clone(),
        now_ms: NOW,
    }
}

// Seed through SQL so consumption tests exercise their own behavior even while
// the new repository's insert method is still a deliberate red-phase stub.
async fn seed(pool: &SqlitePool, row: &FeishuIdentityTransactionRow) {
    sqlx::query(
        "INSERT INTO feishu_identity_transactions \
         (transaction_id,node_id,kind,nonce_hash,browser_binding_hash,redirect_uri,expires_at,consumed_at) \
         VALUES (?,?,?,?,?,?,?,?)",
    )
    .bind(&row.transaction_id)
    .bind(&row.node_id)
    .bind(row.kind.as_str())
    .bind(&row.nonce_hash)
    .bind(&row.browser_binding_hash)
    .bind(&row.redirect_uri)
    .bind(row.expires_at)
    .bind(row.consumed_at)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn valid_transaction_is_persisted_and_consumed_exactly_once() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteFeishuIdentityTransactionRepository::new(db.pool().clone());
    let row = transaction('a');
    let inserted = repo.insert(&row).await;
    assert!(
        inserted.is_ok(),
        "a valid gateway login must be persisted: {inserted:?}"
    );
    assert_eq!(repo.get(&row.transaction_id).await.unwrap(), Some(row.clone()));
    assert!(repo.consume(&admission(&row)).await.unwrap());
    assert!(!repo.consume(&admission(&row)).await.unwrap());
    assert_eq!(
        repo.get(&row.transaction_id).await.unwrap().unwrap().consumed_at,
        Some(NOW)
    );
}

#[tokio::test]
async fn duplicate_identifier_cannot_replace_browser_or_nonce_context() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteFeishuIdentityTransactionRepository::new(db.pool().clone());
    let original = transaction('a');
    seed(db.pool(), &original).await;
    let mut replacement = original.clone();
    replacement.browser_binding_hash = "x".repeat(43);
    let rejected = repo.insert(&replacement).await.unwrap_err();
    assert!(
        rejected.is_unique_violation(),
        "expected a duplicate conflict: {rejected}"
    );
    assert_eq!(repo.get(&original.transaction_id).await.unwrap(), Some(original));
}

#[tokio::test]
async fn every_context_mismatch_is_rejected_without_consuming_valid_transaction() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteFeishuIdentityTransactionRepository::new(db.pool().clone());
    let row = transaction('a');
    seed(db.pool(), &row).await;
    let valid = admission(&row);
    let mut cases = Vec::new();
    let mut wrong = valid.clone();
    wrong.transaction_id = "z".repeat(43);
    cases.push(("transaction", wrong));
    let mut wrong = valid.clone();
    wrong.node_id = "node_other".into();
    cases.push(("node", wrong));
    let mut wrong = valid.clone();
    wrong.kind = FeishuIdentityTransactionKind::H5;
    cases.push(("kind", wrong));
    let mut wrong = valid.clone();
    wrong.nonce_hash = "x".repeat(43);
    cases.push(("nonce", wrong));
    let mut wrong = valid.clone();
    wrong.browser_binding_hash = "x".repeat(43);
    cases.push(("browser", wrong));
    let mut wrong = valid.clone();
    wrong.redirect_uri = "https://other.easiful.com/api/auth/feishu/callback".into();
    cases.push(("redirect", wrong));
    for (label, params) in cases {
        assert!(
            !repo.consume(&params).await.unwrap(),
            "mismatched {label} must be rejected"
        );
    }
    assert!(
        repo.consume(&valid).await.unwrap(),
        "rejected contexts must not consume the original"
    );
}

#[tokio::test]
async fn expiry_is_exclusive_at_exact_deadline() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteFeishuIdentityTransactionRepository::new(db.pool().clone());
    let first = transaction('a');
    let second = transaction('b');
    seed(db.pool(), &first).await;
    seed(db.pool(), &second).await;
    let mut at_deadline = admission(&first);
    at_deadline.now_ms = first.expires_at;
    assert!(!repo.consume(&at_deadline).await.unwrap());
    let mut before_deadline = admission(&second);
    before_deadline.now_ms = second.expires_at - 1;
    assert!(repo.consume(&before_deadline).await.unwrap());
}

#[tokio::test]
async fn concurrent_admission_has_exactly_one_winner() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteFeishuIdentityTransactionRepository::new(db.pool().clone());
    let row = transaction('a');
    seed(db.pool(), &row).await;
    let params = admission(&row);
    let (one, two) = tokio::join!(repo.consume(&params), repo.consume(&params));
    let admitted = one.unwrap() as usize + two.unwrap() as usize;
    assert_eq!(admitted, 1, "concurrent callbacks cannot both issue a session");
}

#[tokio::test]
async fn consumed_state_survives_repository_recreation() {
    let db = init_database_memory().await.unwrap();
    let row = transaction('a');
    seed(db.pool(), &row).await;
    let first = SqliteFeishuIdentityTransactionRepository::new(db.pool().clone());
    assert!(first.consume(&admission(&row)).await.unwrap());
    drop(first);
    let restored = SqliteFeishuIdentityTransactionRepository::new(db.pool().clone());
    assert_eq!(
        restored.get(&row.transaction_id).await.unwrap().unwrap().consumed_at,
        Some(NOW)
    );
    assert!(!restored.consume(&admission(&row)).await.unwrap());
}

#[tokio::test]
async fn cleanup_keeps_live_transactions_and_expires_both_consumed_and_unused_rows() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteFeishuIdentityTransactionRepository::new(db.pool().clone());
    let mut expired = transaction('a');
    expired.expires_at = NOW;
    let mut consumed = transaction('b');
    consumed.expires_at = NOW - 1;
    consumed.consumed_at = Some(NOW - 2);
    let live = transaction('c');
    for row in [&expired, &consumed, &live] {
        seed(db.pool(), row).await;
    }
    assert_eq!(repo.delete_expired(NOW).await.unwrap(), 2);
    assert!(repo.get(&expired.transaction_id).await.unwrap().is_none());
    assert!(repo.get(&consumed.transaction_id).await.unwrap().is_none());
    assert_eq!(repo.get(&live.transaction_id).await.unwrap(), Some(live));
}

#[tokio::test]
async fn insert_rejects_malformed_or_preconsumed_context_without_persisting_it() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteFeishuIdentityTransactionRepository::new(db.pool().clone());
    let valid = transaction('a');
    let mut invalid = Vec::new();
    let mut row = valid.clone();
    row.transaction_id = ".".repeat(43);
    invalid.push(row);
    let mut row = valid.clone();
    row.nonce_hash = "short".into();
    invalid.push(row);
    let mut row = valid.clone();
    row.browser_binding_hash = "!".repeat(43);
    invalid.push(row);
    let mut row = valid.clone();
    row.node_id.clear();
    invalid.push(row);
    let mut row = valid.clone();
    row.node_id = "node\0other".into();
    invalid.push(row);
    let mut row = valid.clone();
    row.node_id = "x".repeat(513);
    invalid.push(row);
    let mut row = valid.clone();
    row.redirect_uri.clear();
    invalid.push(row);
    let mut row = valid.clone();
    row.redirect_uri = "https://qa.easiful.com/\nother".into();
    invalid.push(row);
    let mut row = valid.clone();
    row.redirect_uri = "x".repeat(2049);
    invalid.push(row);
    let mut row = valid.clone();
    row.expires_at = 0;
    invalid.push(row);
    let mut row = valid.clone();
    row.consumed_at = Some(NOW);
    invalid.push(row);
    for row in invalid {
        let error = repo.insert(&row).await.unwrap_err();
        assert!(
            matches!(error, aionui_db::DbError::Init(ref code) if code == "FEISHU_IDENTITY_TRANSACTION_INVALID"),
            "invalid login context must have a fixed rejection code: {error}"
        );
        assert!(repo.get(&row.transaction_id).await.unwrap().is_none());
    }
    repo.insert(&valid).await.unwrap();
    assert_eq!(repo.get(&valid.transaction_id).await.unwrap(), Some(valid));
}

#[tokio::test]
async fn invalid_admission_clock_does_not_consume_valid_transaction() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteFeishuIdentityTransactionRepository::new(db.pool().clone());
    let row = transaction('a');
    seed(db.pool(), &row).await;
    let valid = admission(&row);
    let mut invalid = valid.clone();
    invalid.now_ms = 0;
    assert!(!repo.consume(&invalid).await.unwrap());
    assert!(repo.consume(&valid).await.unwrap());
}

#[test]
fn diagnostic_debug_does_not_expose_transaction_or_browser_material() {
    let row = transaction('a');
    let debug = format!("{row:?}");
    for value in [&row.transaction_id, &row.nonce_hash, &row.browser_binding_hash] {
        assert!(!debug.contains(value));
    }
}

#[tokio::test]
async fn file_database_independent_connection_pools_have_one_admission_winner() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("identity.db");
    let first = aionui_db::init_database(&path).await.unwrap();
    let second = aionui_db::init_database(&path).await.unwrap();
    let first_repo = SqliteFeishuIdentityTransactionRepository::new(first.pool().clone());
    let second_repo = SqliteFeishuIdentityTransactionRepository::new(second.pool().clone());
    let row = transaction('a');
    first_repo.insert(&row).await.unwrap();
    let params = admission(&row);
    // Distinct pools guarantee the two atomic writes use separate connections
    // to the real WAL database, rather than the single in-memory connection.
    let (one, two) = tokio::join!(first_repo.consume(&params), second_repo.consume(&params));
    assert_eq!(usize::from(one.unwrap()) + usize::from(two.unwrap()), 1);
    assert_eq!(
        second_repo.get(&row.transaction_id).await.unwrap().unwrap().consumed_at,
        Some(NOW)
    );
    first.pool().close().await;
    second.pool().close().await;
}

#[tokio::test]
async fn consumed_admission_survives_closing_pool_and_reopening_database_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("identity.db");
    let row = transaction('a');
    {
        let database = aionui_db::init_database(&path).await.unwrap();
        let repo = SqliteFeishuIdentityTransactionRepository::new(database.pool().clone());
        repo.insert(&row).await.unwrap();
        assert!(repo.consume(&admission(&row)).await.unwrap());
        drop(repo);
        database.pool().close().await;
    }
    let reopened = aionui_db::init_database(&path).await.unwrap();
    let repo = SqliteFeishuIdentityTransactionRepository::new(reopened.pool().clone());
    assert_eq!(
        repo.get(&row.transaction_id).await.unwrap().unwrap().consumed_at,
        Some(NOW)
    );
    assert!(!repo.consume(&admission(&row)).await.unwrap());
    reopened.pool().close().await;
}
