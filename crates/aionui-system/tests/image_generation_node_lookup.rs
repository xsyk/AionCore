//! Looking for Node must never hold a session up.
//!
//! `ImageGenerationService::session_server` runs whenever a session is
//! assembled, which is on every message of every conversation, and the server it
//! builds needs Node to run the script. On a machine that has to install a
//! runtime that lookup takes minutes, so the service looks in the background and
//! a session waits for it three seconds at most: a session that cannot have the
//! tool in time starts without it, and one that comes after the lookup finished
//! gets it.
//!
//! Everything here runs on a paused clock, with in-memory repositories: a
//! database worker thread would let the paused clock race ahead of its answers.
//! Node is found by injected lookups that take as long as the test says.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aionui_api_types::{SessionMcpServer, SessionMcpTransport, UpdateImageGenerationSettingsRequest};
use aionui_common::encrypt_string;
use aionui_db::models::Provider;
use aionui_db::{CreateProviderParams, DbError, IGlobalSettingRepository, IProviderRepository, UpdateProviderParams};
use aionui_runtime::ResolvedCommand;
use aionui_system::{IMAGE_GENERATION_SETTING_KEY, ImageGenerationService};
use tokio::time::{Instant, timeout};

const KEY: [u8; 32] = [0x42; 32];
const PROVIDER_ID: &str = "prov-image";
const MODEL: &str = "gpt-image-1";

/// How long a session may wait for Node, and how long a failed lookup is left
/// alone. Spelled out here on purpose: they are the contract.
const WAIT: Duration = Duration::from_secs(3);
const COOLDOWN: Duration = Duration::from_secs(5 * 60);

// ---------------------------------------------------------------------------
// In-memory repositories
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MemorySettings(Mutex<HashMap<String, String>>);

#[async_trait::async_trait]
impl IGlobalSettingRepository for MemorySettings {
    async fn get(&self, key: &str) -> Result<Option<String>, DbError> {
        Ok(self.0.lock().unwrap().get(key).cloned())
    }

    async fn set(&self, key: &str, value: &str) -> Result<(), DbError> {
        self.0.lock().unwrap().insert(key.to_owned(), value.to_owned());
        Ok(())
    }
}

/// A settings table that cannot be read.
struct BrokenSettings;

#[async_trait::async_trait]
impl IGlobalSettingRepository for BrokenSettings {
    async fn get(&self, _key: &str) -> Result<Option<String>, DbError> {
        Err(DbError::Init("the table is unreadable".into()))
    }

    async fn set(&self, _key: &str, _value: &str) -> Result<(), DbError> {
        Err(DbError::Init("the table is unreadable".into()))
    }
}

struct MemoryProviders(Vec<Provider>);

#[async_trait::async_trait]
impl IProviderRepository for MemoryProviders {
    async fn list(&self) -> Result<Vec<Provider>, DbError> {
        Ok(self.0.clone())
    }

    async fn find_by_id(&self, id: &str) -> Result<Option<Provider>, DbError> {
        Ok(self.0.iter().find(|provider| provider.id == id).cloned())
    }

    async fn create(&self, _params: CreateProviderParams<'_>) -> Result<Provider, DbError> {
        unimplemented!("not needed")
    }

    async fn update(&self, _id: &str, _params: UpdateProviderParams<'_>) -> Result<Provider, DbError> {
        unimplemented!("not needed")
    }

    async fn delete(&self, _id: &str) -> Result<(), DbError> {
        unimplemented!("not needed")
    }
}

fn provider() -> Provider {
    Provider {
        id: PROVIDER_ID.to_owned(),
        user_id: "admin".to_owned(),
        platform: "openai".to_owned(),
        name: "Images".to_owned(),
        base_url: "https://img.example.com/v1".to_owned(),
        api_key_encrypted: encrypt_string("sk-test", &KEY).unwrap(),
        models: "[]".to_owned(),
        enabled: true,
        capabilities: "[]".to_owned(),
        context_limit: None,
        model_protocols: None,
        model_enabled: None,
        model_health: None,
        model_settings: "{}".to_owned(),
        bedrock_config: None,
        is_full_url: false,
        created_at: 0,
        updated_at: 0,
    }
}

// ---------------------------------------------------------------------------
// Node lookups
// ---------------------------------------------------------------------------

type Lookup = Pin<Box<dyn Future<Output = Option<ResolvedCommand>> + Send>>;

/// Node lookups that answer `answer` after `delay`, and the count of how many
/// were started.
fn lookup(delay: Duration, answer: Option<ResolvedCommand>) -> (impl Fn() -> Lookup + Send + Sync, Arc<AtomicUsize>) {
    let started = Arc::new(AtomicUsize::new(0));
    let counter = started.clone();
    let resolve = move || -> Lookup {
        counter.fetch_add(1, Ordering::SeqCst);
        let answer = answer.clone();
        Box::pin(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            answer
        })
    };
    (resolve, started)
}

/// Node lookups that never finish.
fn never_finishing_lookup() -> (impl Fn() -> Lookup + Send + Sync, Arc<AtomicUsize>) {
    let started = Arc::new(AtomicUsize::new(0));
    let counter = started.clone();
    let resolve = move || -> Lookup {
        counter.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::pending())
    };
    (resolve, started)
}

/// A Node program that exists on disk for as long as the directory lives.
fn installed_node() -> (tempfile::TempDir, ResolvedCommand) {
    let dir = tempfile::tempdir().unwrap();
    let program = dir.path().join("node");
    std::fs::write(&program, b"").unwrap();
    (dir, ResolvedCommand::plain(program))
}

fn started(lookups: &AtomicUsize) -> usize {
    lookups.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// The service under test
// ---------------------------------------------------------------------------

/// A server with the script installed and one enabled image provider, whose
/// image generation setting is stored as `enabled`.
struct World {
    service: ImageGenerationService,
    /// Keeps the script file alive.
    _script: tempfile::NamedTempFile,
}

fn world<F, Fut>(enabled: bool, resolve: F) -> World
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Option<ResolvedCommand>> + Send + 'static,
{
    let script = tempfile::Builder::new().suffix(".js").tempfile().unwrap();
    let service = service_over(Some(script.path().to_path_buf()), enabled, resolve);
    World {
        service,
        _script: script,
    }
}

fn service_over<F, Fut>(script: Option<PathBuf>, enabled: bool, resolve: F) -> ImageGenerationService
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Option<ResolvedCommand>> + Send + 'static,
{
    let settings = MemorySettings::default();
    settings.0.lock().unwrap().insert(
        IMAGE_GENERATION_SETTING_KEY.to_owned(),
        serde_json::json!({"provider_id": PROVIDER_ID, "model": MODEL, "enabled": enabled}).to_string(),
    );
    ImageGenerationService::new(
        Arc::new(settings),
        Arc::new(MemoryProviders(vec![provider()])),
        KEY,
        script,
    )
    .with_node_resolver(resolve)
}

fn enable_request() -> UpdateImageGenerationSettingsRequest {
    UpdateImageGenerationSettingsRequest {
        provider_id: Some(PROVIDER_ID.to_owned()),
        model: Some(MODEL.to_owned()),
        enabled: true,
    }
}

fn disable_request() -> UpdateImageGenerationSettingsRequest {
    UpdateImageGenerationSettingsRequest {
        enabled: false,
        ..enable_request()
    }
}

/// The program a session server launches.
fn command_of(server: &SessionMcpServer) -> &str {
    match &server.transport {
        SessionMcpTransport::Stdio { command, .. } => command,
        other => panic!("the image generation server is a stdio server, got {other:?}"),
    }
}

/// `session_server()` bounded by an outer timeout, so a session that waits for
/// Node for too long fails the test instead of hanging it.
async fn session_server_within_reason(service: &ImageGenerationService) -> Option<SessionMcpServer> {
    timeout(WAIT * 10, service.session_server())
        .await
        .expect("session_server() must come back without waiting for Node to be found")
}

fn assert_waited(since: Instant, expected: Duration) {
    let waited = since.elapsed();
    assert!(
        waited >= expected && waited <= expected + Duration::from_millis(50),
        "expected to wait {expected:?}, waited {waited:?}"
    );
}

// ===========================================================================
// A session never waits long for Node
// ===========================================================================

#[tokio::test(start_paused = true)]
async fn a_session_waits_three_seconds_at_most_for_a_node_lookup_that_never_finishes() {
    let (resolve, lookups) = never_finishing_lookup();
    let world = world(true, resolve);

    let before = Instant::now();
    let server = session_server_within_reason(&world.service).await;

    assert!(server.is_none(), "no Node, no tool");
    assert_waited(before, WAIT);

    // The next session shares the lookup in flight and gets the same budget.
    let before = Instant::now();
    assert!(session_server_within_reason(&world.service).await.is_none());
    assert_waited(before, WAIT);
    assert_eq!(started(&lookups), 1, "no second lookup while one is running");
}

#[tokio::test(start_paused = true)]
async fn a_failed_node_lookup_is_not_repeated_for_every_session() {
    let (resolve, lookups) = lookup(Duration::ZERO, None);
    let world = world(true, resolve);
    let before = Instant::now();

    for _ in 0..5 {
        assert!(session_server_within_reason(&world.service).await.is_none());
    }
    assert_eq!(started(&lookups), 1, "five sessions, one lookup");
    assert_eq!(before.elapsed(), Duration::ZERO, "no session waited for the failure");

    tokio::time::advance(COOLDOWN - Duration::from_secs(1)).await;
    assert!(session_server_within_reason(&world.service).await.is_none());
    assert_eq!(started(&lookups), 1, "still inside the cooldown");

    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(session_server_within_reason(&world.service).await.is_none());
    assert_eq!(started(&lookups), 2, "the cooldown is over, a session tries again");
    for _ in 0..3 {
        assert!(session_server_within_reason(&world.service).await.is_none());
    }
    assert_eq!(started(&lookups), 2, "and that failure starts a new cooldown");
}

#[tokio::test(start_paused = true)]
async fn a_slow_node_lookup_started_by_one_session_serves_the_next() {
    let (_dir, node) = installed_node();
    let (resolve, lookups) = lookup(Duration::from_secs(5), Some(node.clone()));
    let world = world(true, resolve);
    let start = Instant::now();

    assert!(
        session_server_within_reason(&world.service).await.is_none(),
        "the first session starts without the tool"
    );
    assert_waited(start, WAIT);

    let server = session_server_within_reason(&world.service)
        .await
        .expect("the next session gets the tool as soon as Node is found");
    assert_waited(start, Duration::from_secs(5));
    assert_eq!(Some(command_of(&server)), node.program.to_str());
    assert_eq!(started(&lookups), 1, "the lookup of the first session was reused");
}

#[tokio::test(start_paused = true)]
async fn sessions_that_start_together_share_one_node_lookup() {
    let (_dir, node) = installed_node();
    let (resolve, lookups) = lookup(Duration::from_secs(1), Some(node));
    let world = world(true, resolve);
    let service = &world.service;

    let servers = tokio::join!(
        service.session_server(),
        service.session_server(),
        service.session_server(),
        service.session_server(),
    );

    assert!(servers.0.is_some() && servers.1.is_some() && servers.2.is_some() && servers.3.is_some());
    assert_eq!(started(&lookups), 1);
}

#[tokio::test(start_paused = true)]
async fn a_found_node_is_looked_up_once_for_all_sessions() {
    let (_dir, node) = installed_node();
    let (resolve, lookups) = lookup(Duration::ZERO, Some(node));
    let world = world(true, resolve);

    for _ in 0..5 {
        assert!(session_server_within_reason(&world.service).await.is_some());
    }

    assert_eq!(started(&lookups), 1);
}

// ===========================================================================
// Node is looked up before the first session asks
// ===========================================================================

#[tokio::test(start_paused = true)]
async fn switching_the_setting_on_starts_looking_for_node_right_away() {
    let (_dir, node) = installed_node();
    let (resolve, lookups) = lookup(Duration::from_secs(1), Some(node));
    let world = world(false, resolve);
    assert!(world.service.session_server().await.is_none(), "off");
    assert_eq!(started(&lookups), 0, "no lookup while the setting is off");

    world.service.update(enable_request()).await.expect("the save works");
    // No session asks; the lookup finishes by itself.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(started(&lookups), 1);

    let before = Instant::now();
    assert!(world.service.session_server().await.is_some());
    assert_eq!(before.elapsed(), Duration::ZERO, "Node was ready for the first session");
    assert_eq!(started(&lookups), 1);
}

#[tokio::test(start_paused = true)]
async fn switching_the_setting_off_does_not_look_for_node() {
    let (resolve, lookups) = lookup(Duration::ZERO, None);
    let world = world(true, resolve);

    world.service.update(disable_request()).await.expect("the save works");
    tokio::time::sleep(Duration::from_secs(2)).await;

    assert_eq!(started(&lookups), 0);
}

#[tokio::test(start_paused = true)]
async fn warming_up_looks_for_node_when_the_setting_is_already_on() {
    let (_dir, node) = installed_node();
    let (resolve, lookups) = lookup(Duration::from_secs(1), Some(node));
    let world = world(true, resolve);

    world.service.warm_up().await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(started(&lookups), 1, "looked up in the background");

    let before = Instant::now();
    assert!(world.service.session_server().await.is_some());
    assert_eq!(before.elapsed(), Duration::ZERO, "the first session finds Node ready");
    assert_eq!(started(&lookups), 1);
}

#[tokio::test(start_paused = true)]
async fn warming_up_leaves_node_alone_while_the_setting_is_off() {
    let (resolve, lookups) = lookup(Duration::ZERO, None);
    let world = world(false, resolve);

    world.service.warm_up().await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    assert_eq!(started(&lookups), 0);
}

#[tokio::test(start_paused = true)]
async fn warming_up_leaves_node_alone_on_a_server_without_the_script() {
    let (resolve, lookups) = lookup(Duration::ZERO, None);
    let service = service_over(None, true, resolve);

    service.warm_up().await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    assert_eq!(started(&lookups), 0, "nothing could use Node here");
}

#[tokio::test(start_paused = true)]
async fn warming_up_leaves_node_alone_when_the_setting_cannot_be_read() {
    let (resolve, lookups) = lookup(Duration::ZERO, None);
    let script = tempfile::Builder::new().suffix(".js").tempfile().unwrap();
    let service = ImageGenerationService::new(
        Arc::new(BrokenSettings),
        Arc::new(MemoryProviders(vec![provider()])),
        KEY,
        Some(script.path().to_path_buf()),
    )
    .with_node_resolver(resolve);

    service.warm_up().await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    assert_eq!(
        started(&lookups),
        0,
        "a startup that cannot read the setting does not guess"
    );
}
