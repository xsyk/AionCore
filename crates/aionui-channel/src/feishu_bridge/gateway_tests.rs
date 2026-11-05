use super::*;
use aionui_db::{DbError, IFeishuGatewayRepository, SqliteFeishuGatewayRepository};
use aionui_db::models::{FeishuBindingRow, FeishuGatewayDeliveryRow, FeishuGatewayInteractionRow, FeishuGatewayRouteRow};
use crate::gateway::{GatewayClient, GatewayConfig, GatewayDelivery, GatewayRoute};
use axum::{Json, Router, extract::{Path, Query, State}, http::StatusCode, routing::{get, post}};

struct CompletionFailureRepository {
    inner: Arc<SqliteFeishuGatewayRepository>,
    fail: AtomicBool,
    fail_fences: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl IFeishuGatewayRepository for CompletionFailureRepository {
    async fn ensure_identity(&self, node: &str, app: &str, tenant: &str) -> Result<(), DbError> { self.inner.ensure_identity(node, app, tenant).await }
    async fn prepare_route(&self, route: &FeishuGatewayRouteRow) -> Result<(), DbError> { self.inner.prepare_route(route).await }
    async fn activate_route(&self, route: &str, generation: &str) -> Result<FeishuBindingRow, DbError> { self.inner.activate_route(route, generation).await }
    async fn fence_route(&self, route: &str, generation: &str) -> Result<(), DbError> {
        if self.fail_fences.fetch_update(Ordering::SeqCst,Ordering::SeqCst,|count|count.checked_sub(1)).is_ok() {
            return Err(DbError::Conflict("Injected fence failure".into()));
        }
        self.inner.fence_route(route,generation).await
    }
    async fn route_by_generation(&self, generation: &str) -> Result<Option<FeishuGatewayRouteRow>, DbError> { self.inner.route_by_generation(generation).await }
    async fn list_routes(&self) -> Result<Vec<FeishuGatewayRouteRow>, DbError> { self.inner.list_routes().await }
    async fn enqueue_delivery(&self, id: &str, route: &str, generation: &str, kind: &str, payload: &str) -> Result<(), DbError> { self.inner.enqueue_delivery(id, route, generation, kind, payload).await }
    async fn pending_deliveries(&self) -> Result<Vec<FeishuGatewayDeliveryRow>, DbError> { self.inner.pending_deliveries().await }
    async fn start_delivery(&self, id: &str, epoch: &str) -> Result<bool, DbError> { self.inner.start_delivery(id, epoch).await }
    async fn complete_delivery(&self, id: &str, outcome: &str) -> Result<(), DbError> {
        if outcome == "handled" && self.fail.swap(false, Ordering::SeqCst) { return Err(DbError::Conflict("Injected completion failure".into())); }
        self.inner.complete_delivery(id, outcome).await
    }
    async fn save_interaction(&self, token: &str, binding: &str, payload: &str, expires: i64) -> Result<(), DbError> { self.inner.save_interaction(token, binding, payload, expires).await }
    async fn list_interactions(&self, now: i64) -> Result<Vec<FeishuGatewayInteractionRow>, DbError> { self.inner.list_interactions(now).await }
    async fn delete_interaction(&self, token: &str) -> Result<(), DbError> { self.inner.delete_interaction(token).await }
}

struct Hub {
    route: Mutex<GatewayRoute>,
    extra_routes: Mutex<HashMap<String, GatewayRoute>>,
    deliveries: Mutex<Vec<GatewayDelivery>>,
    acks: Mutex<Vec<(String, String)>>,
    outbound: Mutex<Vec<Value>>,
    outbound_state: Mutex<String>,
    revocations: Mutex<Vec<String>>,
    fail_revoke: AtomicBool,
    activate_on_ack: AtomicBool,
    reject_poll: AtomicBool,
    fail_prepare_ack: AtomicBool,
    fail_fences: Arc<AtomicUsize>,
    rejections: Mutex<Vec<String>>,
    repository: Arc<dyn IFeishuGatewayRepository>,
    bindings: Arc<SqliteFeishuBindingRepository>,
    owner: String,
}

impl Hub {
    fn route_for_conversation(&self, conversation: &str) -> Option<GatewayRoute> {
        let route = self.route.lock().unwrap().clone();
        if route.conversation_id == conversation { Some(route) } else { self.extra_routes.lock().unwrap().get(conversation).cloned() }
    }
    fn set_route_status(&self, id: &str, status: &str) -> GatewayRoute {
        let mut route = self.route.lock().unwrap();
        if route.id == id {
            route.status = status.into();
            if status == "active" { route.prepared_at = Some(now_ms()); }
            return route.clone();
        }
        drop(route);
        let mut extra = self.extra_routes.lock().unwrap();
        let route = extra.values_mut().find(|route| route.id == id).unwrap();
        route.status = status.into();
        if status == "active" { route.prepared_at = Some(now_ms()); }
        route.clone()
    }
}

fn route() -> GatewayRoute {
    serde_json::from_value(json!({"id":"route","generation":"generation","node_id":"node","conversation_id":"test_conversation",
        "owner_union_id":"on_owner","app_id":"app","tenant_key":"tenant","chat_id":"group","chat_type":"group","status":"preparing","created_at":now_ms()})).unwrap()
}

fn delivery(route: &GatewayRoute, id: &str, kind: &str, event: Option<Value>) -> GatewayDelivery {
    serde_json::from_value(json!({"id":id,"kind":kind,"route":route,"event":event,"created_at":now_ms(),"expires_at":if kind.starts_with("binding.") {0} else {now_ms()+300000}})).unwrap()
}

fn message_event(id: &str, text: &str) -> Value {
    json!({"id":id,"kind":"message","app_id":"app","tenant_key":"tenant","chat_id":"group","chat_type":"group",
        "actor_open_id":"member","actor_union_id":"on_other","actor_type":"user","mentioned_bot":true,"text":text,"occurred_at":now_ms()})
}

fn bridge_with_gateway(fixture: &Fixture, url: &str, repository: Arc<dyn IFeishuGatewayRepository>) -> Arc<FeishuBridge> {
    let deps = &fixture.bridge.deps;
    Arc::new(FeishuBridge::new(FeishuBridgeDependencies {
        bindings: deps.bindings.clone(), conversations: deps.conversations.clone(), conversation_repo: deps.conversation_repo.clone(),
        users: deps.users.clone(), login_config: deps.login_config.clone(), tasks: deps.tasks.clone(), sender: deps.sender.clone(),
        gateway: Some(Arc::new(GatewayClient::new(GatewayConfig { url:url.into(), node_id:"node".into(), app_id:"app".into(), tenant_key:"tenant".into(), token:"gateway-unit-test-credential-at-least-32-chars".into() }).unwrap())),
        gateway_repository: Some(repository),
    }))
}

async fn gateway_fixture(fail_completion: bool) -> (Fixture, Arc<Hub>, String, tokio::task::JoinHandle<()>) {
    async fn routes(State(hub): State<Arc<Hub>>, Query(query): Query<HashMap<String, String>>) -> (StatusCode, Json<Value>) {
        match hub.route_for_conversation(query.get("conversation_id").unwrap()) {
            Some(route) => (StatusCode::OK, Json(json!({"route":route}))),
            None => (StatusCode::NOT_FOUND, Json(json!({"error":{"code":"not_found"}}))),
        }
    }
    async fn deliveries(State(hub): State<Arc<Hub>>) -> (StatusCode,Json<Value>) {
        if hub.reject_poll.load(Ordering::SeqCst) { return (StatusCode::UNAUTHORIZED,Json(json!({"error":{"code":"unauthorized"}}))); }
        (StatusCode::OK,Json(json!({"deliveries":hub.deliveries.lock().unwrap().clone()})))
    }
    async fn ack(State(hub): State<Arc<Hub>>, Path(id): Path<String>, Json(body): Json<Value>) -> (StatusCode,Json<Value>) {
        let status = body["status"].as_str().unwrap().to_owned();
        if status == "prepared" && hub.fail_prepare_ack.swap(false,Ordering::SeqCst) {
            return (StatusCode::SERVICE_UNAVAILABLE,Json(json!({"error":{"code":"unavailable"}})));
        }
        let route = { hub.deliveries.lock().unwrap().iter().find(|delivery| delivery.id == id).map(|delivery| delivery.route.clone()).unwrap_or_else(|| hub.route.lock().unwrap().clone()) };
        match status.as_str() {
            "prepared" => {
                assert!(hub.repository.route_by_generation(&route.generation).await.unwrap().is_some());
                if hub.activate_on_ack.load(Ordering::SeqCst) { hub.set_route_status(&route.id,"active"); }
            }
            "revoked" => {
                assert_eq!(hub.repository.route_by_generation(&route.generation).await.unwrap().unwrap().status,"fenced");
                assert!(hub.bindings.get_by_conversation(&hub.owner,&route.conversation_id).await.unwrap().is_none());
                hub.set_route_status(&route.id,"revoked");
            }
            "accepted" => {
                let already = { hub.acks.lock().unwrap().iter().any(|(seen, kind)| seen == &id && kind == "accepted") };
                if !already { assert!(hub.repository.pending_deliveries().await.unwrap().iter().any(|row| row.id == id)); }
            }
            _ => panic!("invalid ACK"),
        }
        hub.acks.lock().unwrap().push((id.clone(),status));
        hub.deliveries.lock().unwrap().retain(|delivery| delivery.id != id);
        (StatusCode::OK,Json(json!({"acknowledged":true})))
    }
    async fn outbound(State(hub): State<Arc<Hub>>, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
        let key = body["idempotency_key"].clone();
        let interaction = body["interaction"]["id"].clone();
        let route = body["route_id"].clone(); let generation = body["generation"].clone();
        let state = hub.outbound_state.lock().unwrap().clone();
        let platform_id = if state == "delivered" { Some("platform_card") } else { None };
        hub.outbound.lock().unwrap().push(body);
        (StatusCode::ACCEPTED,Json(json!({"outbound":{"id":"outbound","node_id":"node","route_id":route,"generation":generation,"idempotency_key":key,"state":state,"platform_message_id":platform_id,"interaction_id":interaction}})))
    }
    async fn revoke(State(hub): State<Arc<Hub>>, Path(id): Path<String>) -> (StatusCode, Json<Value>) {
        if hub.fail_revoke.swap(false,Ordering::SeqCst) { return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error":{"code":"unavailable"}}))); }
        let route = hub.set_route_status(&id,"revoking");
        hub.revocations.lock().unwrap().push(id.clone());
        hub.deliveries.lock().unwrap().push(delivery(&route,&format!("revoke_{id}"),"binding.revoke",None));
        (StatusCode::OK, Json(json!({"route":route})))
    }
    async fn reject(State(hub): State<Arc<Hub>>, Path(id): Path<String>) -> Json<Value> {
        let route = {
            let mut route = hub.route.lock().unwrap();
            assert_eq!(id,route.id); assert!(matches!(route.status.as_str(),"preparing"|"revoked"));
            route.status = "revoked".into(); route.clone()
        };
        hub.rejections.lock().unwrap().push(id.clone());
        hub.deliveries.lock().unwrap().retain(|delivery|delivery.route.id != id);
        Json(json!({"route":route}))
    }
    let mut fixture = fixture().await;
    let inner = Arc::new(SqliteFeishuGatewayRepository::new(fixture._database.pool().clone()));
    let fail_fences = Arc::new(AtomicUsize::new(0));
    let repository: Arc<dyn IFeishuGatewayRepository> = Arc::new(CompletionFailureRepository { inner, fail:AtomicBool::new(fail_completion), fail_fences:fail_fences.clone() });
    repository.ensure_identity("node","app","tenant").await.unwrap();
    let route = route();
    let hub = Arc::new(Hub { route:Mutex::new(route.clone()), extra_routes:Mutex::new(HashMap::new()), deliveries:Mutex::new(vec![delivery(&route,"prepare","binding.prepare",None)]), acks:Mutex::new(Vec::new()),outbound:Mutex::new(Vec::new()),outbound_state:Mutex::new("delivered".into()),revocations:Mutex::new(Vec::new()),fail_revoke:AtomicBool::new(false),activate_on_ack:AtomicBool::new(true),reject_poll:AtomicBool::new(false),fail_prepare_ack:AtomicBool::new(false),fail_fences,rejections:Mutex::new(Vec::new()),repository:repository.clone(),bindings:fixture.bindings.clone(),owner:fixture.owner.clone() });
    let router = Router::new().route("/api/node/route",get(routes)).route("/api/node/deliveries",get(deliveries))
        .route("/api/node/deliveries/{id}/ack",post(ack)).route("/api/node/routes/{id}/reject",post(reject)).route("/api/node/routes/{id}/revoke",post(revoke)).route("/api/node/outbound",post(outbound)).with_state(hub.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}",listener.local_addr().unwrap());
    let task = tokio::spawn(async move {axum::serve(listener,router).await.unwrap()});
    fixture.bridge = bridge_with_gateway(&fixture,&url,repository);
    (fixture,hub,url,task)
}

#[tokio::test]
async fn gateway_prepare_replay_activation_and_revoke_require_durable_local_state() {
    let (fixture,hub,_,task) = gateway_fixture(false).await;
    hub.activate_on_ack.store(false,Ordering::SeqCst);
    fixture.bridge.flush_gateway().await.unwrap();
    assert!(fixture.bindings.get_by_conversation(&fixture.owner,"test_conversation").await.unwrap().is_none());
    assert_eq!(hub.repository.route_by_generation("generation").await.unwrap().unwrap().status,"prepared");
    let prepare = delivery(&hub.route.lock().unwrap().clone(),"prepare","binding.prepare",None);
    fixture.bridge.accept_gateway_delivery(&prepare).await.unwrap();
    hub.route.lock().unwrap().status = "active".into();
    fixture.bridge.flush_gateway().await.unwrap();
    assert_eq!(fixture.bindings.get_by_conversation(&fixture.owner,"test_conversation").await.unwrap().unwrap().id,"generation");
    let mut wrong = delivery(&hub.route.lock().unwrap().clone(),"wrong","message",Some(message_event("wrong","hello")));
    wrong.route.generation = "other_generation".into();
    let before = hub.acks.lock().unwrap().len();
    assert!(fixture.bridge.accept_gateway_delivery(&wrong).await.is_err());
    assert_eq!(hub.acks.lock().unwrap().len(),before);
    let mut bad_owner = prepare.clone(); bad_owner.route.owner_union_id = "on_other".into();
    assert!(fixture.bridge.accept_gateway_delivery(&bad_owner).await.is_err());
    hub.route.lock().unwrap().status = "revoking".into();
    let revoke = delivery(&hub.route.lock().unwrap().clone(),"revoke","binding.revoke",None);
    fixture.bridge.accept_gateway_delivery(&revoke).await.unwrap();
    fixture.bridge.accept_gateway_delivery(&revoke).await.unwrap();
    assert_eq!(hub.repository.route_by_generation("generation").await.unwrap().unwrap().status,"fenced");
    assert!(fixture.bindings.get_by_conversation(&fixture.owner,"test_conversation").await.unwrap().is_none());
    fixture.bridge.accept_gateway_delivery(&prepare).await.unwrap();
    task.abort();
}

async fn permission(fixture: &Fixture) -> String {
    let binding = fixture.bindings.get_by_conversation(&fixture.owner,"test_conversation").await.unwrap().unwrap();
    let confirmation = Confirmation { id:"permission".into(),call_id:"permission".into(),title:None,action:None,description:"Proceed".into(),command_type:None,questions:None,
        options:vec![ConfirmationOption {label:"Allow".into(),value:json!({"opaque":"allow"}),params:None}] };
    fixture.agent.confirmations.lock().unwrap().push(confirmation.clone());
    fixture.bridge.publish_confirmation(&binding,"message",confirmation).await.unwrap();
    fixture.bridge.flush_interactions().await;
    let pending = fixture.bridge.interactions.lock().await;
    assert_eq!(pending.len(),1);
    assert_eq!(pending.values().next().unwrap().message_id.as_deref(),Some("platform_card"));
    pending.keys().next().unwrap().clone()
}

fn click(route: &GatewayRoute, id: &str, token: &str) -> GatewayDelivery {
    delivery(route,id,"interaction",Some(json!({"id":id,"kind":"interaction","app_id":"app","tenant_key":"tenant","chat_id":"group","chat_type":"group","actor_open_id":"group_member","actor_type":"user","interaction_id":token,"platform_message_id":"platform_card","action":"feishu.confirm","occurred_at":now_ms(),"value":{"tag":"button","value":{"action":"feishu.confirm","params":{"token":token,"option":"0"}}}})))
}

#[tokio::test]
async fn gateway_restores_card_and_does_not_repeat_confirmation_after_completion_write_failure() {
    let (mut fixture,hub,url,task) = gateway_fixture(true).await;
    fixture.bridge.flush_gateway().await.unwrap();
    let token = permission(&fixture).await;
    let body = hub.outbound.lock().unwrap()[0].clone();
    assert_eq!(body["interaction"]["id"],token); assert_eq!(body["interaction"]["request_id"],"permission");
    fixture.bridge = bridge_with_gateway(&fixture,&url,hub.repository.clone());
    fixture.bridge.restore_gateway_interactions().await.unwrap();
    assert!(fixture.bridge.interactions.lock().await.contains_key(&token));
    let click = click(&hub.route.lock().unwrap().clone(),"click",&token);
    hub.deliveries.lock().unwrap().push(click.clone());
    assert!(fixture.bridge.flush_gateway().await.is_err());
    assert_eq!(fixture.agent.decisions.lock().unwrap().len(),1);
    assert_eq!(hub.repository.pending_deliveries().await.unwrap().len(),1);
    assert_eq!(fixture.bridge.gateway_attempts.lock().await.get("click").map(String::as_str),Some("handled"));
    fixture.bridge.flush_gateway().await.unwrap();
    assert_eq!(fixture.agent.decisions.lock().unwrap().len(),1);
    assert!(hub.repository.pending_deliveries().await.unwrap().is_empty());
    assert!(!fixture.bridge.gateway_attempts.lock().await.contains_key("click"));
    assert!(hub.repository.list_interactions(now_ms()).await.unwrap().is_empty());
    hub.deliveries.lock().unwrap().push(click);
    fixture.bridge.flush_gateway().await.unwrap();
    assert_eq!(fixture.agent.decisions.lock().unwrap().len(),1);
    assert!(fixture.sender.sent.lock().unwrap().is_empty());
    task.abort();
}

#[tokio::test]
async fn gateway_interrupted_interaction_is_unknown_without_reexecution() {
    let (fixture,hub,_,task) = gateway_fixture(false).await;
    fixture.bridge.flush_gateway().await.unwrap();
    let token = permission(&fixture).await;
    let click = click(&hub.route.lock().unwrap().clone(),"interrupted",&token);
    fixture.bridge.accept_gateway_delivery(&click).await.unwrap();
    assert!(hub.repository.start_delivery("interrupted","previous_process").await.unwrap());
    fixture.bridge.flush_gateway().await.unwrap();
    assert!(fixture.agent.decisions.lock().unwrap().is_empty());
    assert!(hub.repository.pending_deliveries().await.unwrap().is_empty());
    assert!(hub.repository.list_interactions(now_ms()).await.unwrap().is_empty());
    task.abort();
}

#[tokio::test]
async fn gateway_message_receipt_replay_enqueues_one_existing_conversation_input() {
    let (fixture,hub,_,task) = gateway_fixture(false).await;
    fixture.bridge.flush_gateway().await.unwrap();
    let delivery = delivery(&hub.route.lock().unwrap().clone(),"message_delivery","message",Some(message_event("user_event","hello existing conversation")));
    hub.deliveries.lock().unwrap().push(delivery.clone());
    fixture.bridge.flush_gateway().await.unwrap();
    assert_eq!(fixture.bindings.pending_inbox().await.unwrap().len(),1);
    hub.deliveries.lock().unwrap().push(delivery);
    fixture.bridge.flush_gateway().await.unwrap();
    assert_eq!(fixture.bindings.pending_inbox().await.unwrap().len(),1);
    fixture.bridge.flush_inbox().await.unwrap();
    let message = fixture.bridge.deps.conversation_repo.get_message(&fixture.owner,"test_conversation","feishu:app:user_event").await.unwrap().unwrap();
    assert!(message.content.contains("hello existing conversation"));
    assert!(fixture.bindings.pending_inbox().await.unwrap().is_empty());
    task.abort();
}

#[tokio::test]
async fn gateway_question_selection_survives_bridge_restart() {
    let (mut fixture,hub,url,task) = gateway_fixture(false).await;
    fixture.bridge.flush_gateway().await.unwrap();
    let binding = fixture.bindings.get_by_conversation(&fixture.owner,"test_conversation").await.unwrap().unwrap();
    fixture.bridge.publish_question(&binding,"message",&json!({"request_id":"ask","questions":[{"question":"Which","multi_select":true,"options":[{"label":"A"},{"label":"B"}]}]})).await.unwrap();
    fixture.bridge.flush_interactions().await;
    let token = fixture.bridge.interactions.lock().await.keys().next().unwrap().clone();
    let mut select = click(&hub.route.lock().unwrap().clone(),"select",&token);
    let event = select.event.as_mut().unwrap();
    event.action = "feishu.select".into();
    event.value = json!({"tag":"button","value":{"action":"feishu.select","params":{"token":token,"question":"0","option":"1"}}});
    hub.deliveries.lock().unwrap().push(select);
    fixture.bridge.flush_gateway().await.unwrap();
    let persisted = hub.repository.list_interactions(now_ms()).await.unwrap();
    assert_eq!(persisted.len(),1);
    let state: Value = serde_json::from_str(&persisted[0].payload).unwrap();
    assert_eq!(state["kind"]["Ask"][0]["selected"],json!(["B"]));
    fixture.bridge = bridge_with_gateway(&fixture,&url,hub.repository.clone());
    fixture.bridge.restore_gateway_interactions().await.unwrap();
    let restored = fixture.bridge.interactions.lock().await;
    let state = serde_json::to_value(restored.get(&token).unwrap()).unwrap();
    assert_eq!(state["kind"]["Ask"][0]["selected"],json!(["B"]));
    drop(restored);
    task.abort();
}

#[tokio::test]
async fn gateway_auth_rejection_fences_local_generation_without_fake_revoke_ack() {
    let (fixture,hub,_,task) = gateway_fixture(false).await;
    fixture.bridge.flush_gateway().await.unwrap();
    let message = delivery(&hub.route.lock().unwrap().clone(),"auth_pending","message",Some(message_event("auth_pending","must not execute")));
    fixture.bridge.accept_gateway_delivery(&message).await.unwrap();
    fixture.bindings.enqueue_inbox("feishu:app:already_queued","generation","must not execute").await.unwrap();
    hub.reject_poll.store(true,Ordering::SeqCst);
    assert!(fixture.bridge.flush_gateway().await.is_err());
    assert!(fixture.bridge.flush_inbox().await.is_err());
    assert_eq!(hub.repository.route_by_generation("generation").await.unwrap().unwrap().status,"fenced");
    assert!(fixture.bindings.get_by_conversation(&fixture.owner,"test_conversation").await.unwrap().is_none());
    assert!(fixture.bridge.deps.conversation_repo.get_message(&fixture.owner,"test_conversation","feishu:app:already_queued").await.unwrap().is_none());
    assert!(!hub.acks.lock().unwrap().iter().any(|(_,status)| status == "revoked"));
    assert!(fixture.agent.decisions.lock().unwrap().is_empty());
    task.abort();
}

#[tokio::test]
async fn gateway_rejected_preparation_recovers_fence_failure_without_repreparing_or_fake_ack() {
    let (fixture,hub,_,task) = gateway_fixture(false).await;
    hub.fail_prepare_ack.store(true,Ordering::SeqCst);
    let prepare = hub.deliveries.lock().unwrap()[0].clone();
    fixture.bridge.flush_gateway().await.unwrap();
    assert_eq!(hub.repository.route_by_generation("generation").await.unwrap().unwrap().status,"prepared");
    fixture.bridge.deps.users.set_status(&fixture.owner,UserStatus::Disabled).await.unwrap();
    hub.fail_fences.store(2,Ordering::SeqCst);
    fixture.bridge.flush_gateway().await.unwrap();
    assert_eq!(hub.rejections.lock().unwrap().len(),1);
    fixture.bridge.flush_gateway().await.unwrap();
    assert_eq!(hub.repository.route_by_generation("generation").await.unwrap().unwrap().status,"fenced");
    fixture.bridge.accept_gateway_delivery(&prepare).await.unwrap();
    assert_eq!(hub.rejections.lock().unwrap().len(),1);
    assert!(hub.acks.lock().unwrap().is_empty());
    assert!(fixture.bindings.get_by_conversation(&fixture.owner,"test_conversation").await.unwrap().is_none());
    task.abort();
}


async fn old_gateway_outbox(fixture: &Fixture, id: &str) {
    fixture.bindings.enqueue_outbox(&[aionui_db::models::FeishuOutboxRow {
        id:id.into(),binding_id:"generation".into(),message_id:format!("message_{id}"),part:0,
        content:"reply after network recovery".into(),platform_message_id:None,first_attempt_at:None,
    }]).await.unwrap();
    fixture.bindings.start_outbox_attempt(id,now_ms()-60*60*1000).await.unwrap();
}

#[tokio::test]
async fn gateway_old_local_attempt_without_hub_receipt_still_submits_stable_key() {
    let (fixture,hub,_,task) = gateway_fixture(false).await;
    fixture.bridge.flush_gateway().await.unwrap();
    old_gateway_outbox(&fixture,"network_interrupted").await;
    assert!(hub.outbound.lock().unwrap().is_empty());
    fixture.bridge.flush_deliveries().await.unwrap();
    {
        let sent = hub.outbound.lock().unwrap();
        assert_eq!(sent.len(),1);
        assert_eq!(sent[0]["idempotency_key"],"network_interrupted");
        assert_eq!(sent[0]["route_id"],"route");
        assert_eq!(sent[0]["generation"],"generation");
    }
    assert!(fixture.bindings.pending_outbox().await.unwrap().is_empty());
    assert!(fixture.sender.sent.lock().unwrap().is_empty());
    task.abort();
}

#[tokio::test]
async fn gateway_terminal_unknown_and_canceled_receipts_never_complete_local_outbox() {
    for state in ["unknown","canceled"] {
        let (fixture,hub,_,task) = gateway_fixture(false).await;
        fixture.bridge.flush_gateway().await.unwrap();
        *hub.outbound_state.lock().unwrap() = state.into();
        old_gateway_outbox(&fixture,"uncertain_reply").await;
        fixture.bridge.flush_deliveries().await.unwrap();
        fixture.bridge.flush_deliveries().await.unwrap();
        let pending = fixture.bindings.pending_outbox().await.unwrap();
        assert_eq!(pending.len(),1);
        assert_eq!(pending[0].id,"uncertain_reply");
        assert!(pending[0].platform_message_id.is_none());
        let outbound = hub.outbound.lock().unwrap();
        assert_eq!(outbound.len(),2);
        assert!(outbound.iter().all(|body|body["idempotency_key"] == "uncertain_reply"));
        assert!(fixture.sender.sent.lock().unwrap().is_empty());
        task.abort();
    }
}

#[tokio::test]
async fn gateway_prepared_owner_disabled_fences_only_that_route_and_healthy_queue_progresses() {
    let (fixture,hub,_,task) = gateway_fixture(false).await;
    let prepare = hub.deliveries.lock().unwrap()[0].clone();
    fixture.bridge.accept_gateway_delivery(&prepare).await.unwrap();
    assert_eq!(hub.repository.route_by_generation("generation").await.unwrap().unwrap().status,"prepared");
    let mut conversation = fixture.bridge.deps.conversation_repo.get(&fixture.owner,"test_conversation").await.unwrap().unwrap();
    conversation.id = "healthy_conversation".into(); conversation.user_id = fixture.other.clone();
    fixture.bridge.deps.conversation_repo.create(&conversation).await.unwrap();
    let mut healthy = route(); healthy.id="healthy_route".into(); healthy.generation="healthy_generation".into(); healthy.conversation_id=conversation.id.clone(); healthy.owner_union_id="on_other".into(); healthy.chat_id="healthy_group".into();
    hub.extra_routes.lock().unwrap().insert(conversation.id.clone(),healthy.clone());
    let prepare = delivery(&healthy,"healthy_prepare","binding.prepare",None);
    hub.deliveries.lock().unwrap().push(prepare.clone());
    fixture.bridge.accept_gateway_delivery(&prepare).await.unwrap();
    healthy = hub.route_for_conversation(&conversation.id).unwrap();
    hub.repository.activate_route(&healthy.id,&healthy.generation).await.unwrap();
    let mut event = message_event("healthy_message","healthy input"); event["chat_id"] = json!("healthy_group");
    hub.deliveries.lock().unwrap().push(delivery(&healthy,"healthy_message_delivery","message",Some(event)));
    fixture.bridge.deps.users.set_status(&fixture.owner,UserStatus::Disabled).await.unwrap();
    fixture.bridge.flush_gateway().await.unwrap();
    assert_eq!(hub.repository.route_by_generation("generation").await.unwrap().unwrap().status,"fenced");
    assert_eq!(hub.route.lock().unwrap().status,"revoking");
    assert_eq!(hub.revocations.lock().unwrap().as_slice(),["route"]);
    assert_eq!(hub.repository.route_by_generation("healthy_generation").await.unwrap().unwrap().status,"active");
    let inbox = fixture.bindings.pending_inbox().await.unwrap();
    assert_eq!(inbox.len(),1); assert_eq!(inbox[0].binding_id,"healthy_generation");
    assert!(!hub.acks.lock().unwrap().iter().any(|(_,status)|status=="revoked"));
    task.abort();
}

#[tokio::test]
async fn gateway_auth_recovery_revokes_fenced_generation_before_new_binding() {
    let (fixture,hub,_,task) = gateway_fixture(false).await;
    fixture.bridge.flush_gateway().await.unwrap();
    hub.reject_poll.store(true,Ordering::SeqCst);
    assert!(fixture.bridge.flush_gateway().await.is_err());
    hub.reject_poll.store(false,Ordering::SeqCst);
    hub.fail_revoke.store(true,Ordering::SeqCst);
    fixture.bridge.flush_gateway().await.unwrap();
    assert_eq!(hub.repository.route_by_generation("generation").await.unwrap().unwrap().status,"fenced");
    assert_eq!(hub.route.lock().unwrap().status,"active");
    fixture.bridge.flush_gateway().await.unwrap();
    assert_eq!(hub.route.lock().unwrap().status,"revoking");
    assert!(!hub.acks.lock().unwrap().iter().any(|(_,status)|status=="revoked"));
    fixture.bridge.flush_gateway().await.unwrap();
    assert_eq!(hub.route.lock().unwrap().status,"revoked");
    assert!(hub.acks.lock().unwrap().iter().any(|(id,status)|id=="revoke_route" && status=="revoked"));
    let mut next = route(); next.id="new_route".into(); next.generation="new_generation".into();
    *hub.route.lock().unwrap() = next.clone();
    hub.deliveries.lock().unwrap().push(delivery(&next,"new_prepare","binding.prepare",None));
    fixture.bridge.flush_gateway().await.unwrap();
    assert_eq!(fixture.bindings.get_by_conversation(&fixture.owner,"test_conversation").await.unwrap().unwrap().id,"new_generation");
    assert_eq!(hub.repository.route_by_generation("generation").await.unwrap().unwrap().status,"fenced");
    task.abort();
}

fn text_answer(route: &GatewayRoute, id: &str, token: &str, text: &str) -> GatewayDelivery {
    // Start with the literal HTTP response captured from the real Go service,
    // where the unrelated new user message ID is absent from the wire event.
    let fixture: Value = serde_json::from_str(include_str!("../../tests/fixtures/feishu_gateway_control.json")).unwrap();
    let response = fixture["responses"].as_array().unwrap().iter().find(|response|response["name"]=="answer").unwrap();
    let body: Value = serde_json::from_str(response["body"].as_str().unwrap()).unwrap();
    let mut answer = body["deliveries"][0].clone();
    assert!(answer["event"].get("platform_message_id").is_none());
    answer["id"] = json!(id); answer["route"] = serde_json::to_value(route).unwrap();
    answer["created_at"] = json!(now_ms()); answer["expires_at"] = json!(now_ms()+300000);
    answer["event"]["id"] = json!(id); answer["event"]["app_id"] = json!(route.app_id); answer["event"]["tenant_key"] = json!(route.tenant_key);
    answer["event"]["chat_id"] = json!(route.chat_id); answer["event"]["chat_type"] = json!(route.chat_type);
    answer["event"]["interaction_id"] = json!(token); answer["event"]["text"] = json!(text); answer["event"]["occurred_at"] = json!(now_ms());
    serde_json::from_value(answer).unwrap()
}

#[tokio::test]
async fn gateway_text_answer_without_card_id_restores_question_and_completes_once() {
    let (mut fixture,hub,url,task) = gateway_fixture(false).await;
    fixture.bridge.flush_gateway().await.unwrap();
    let binding = fixture.bindings.get_by_conversation(&fixture.owner,"test_conversation").await.unwrap().unwrap();
    fixture.bridge.publish_question(&binding,"question_message",&json!({"request_id":"text_question","questions":[
        {"question":"First","options":[{"label":"A"}]},
        {"question":"Second","multi_select":true,"options":[{"label":"B"},{"label":"C"}]}
    ]})).await.unwrap();
    fixture.bridge.flush_interactions().await;
    let token = fixture.bridge.interactions.lock().await.keys().next().unwrap().clone();
    fixture.bridge = bridge_with_gateway(&fixture,&url,hub.repository.clone());
    let route = hub.route.lock().unwrap().clone();
    // Go preserves the text and interaction ID but deliberately clears the new
    // user's message ID: it is not the original question card message ID.
    let answer = text_answer(&route,"text_answer",&token,&format!("/answer  \t{token}\tA | B, C"));
    hub.deliveries.lock().unwrap().push(answer.clone());
    fixture.bridge.flush_gateway().await.unwrap();
    assert_eq!(*fixture.agent.answers.lock().unwrap(),vec![Some(vec![
        ("First".into(),vec!["A".into()]),("Second".into(),vec!["B".into(),"C".into()])
    ])]);
    assert!(hub.acks.lock().unwrap().iter().any(|(id,status)|id=="text_answer" && status=="accepted"));
    assert!(hub.repository.list_interactions(now_ms()).await.unwrap().is_empty());
    assert!(hub.repository.pending_deliveries().await.unwrap().is_empty());
    fixture.bridge = bridge_with_gateway(&fixture,&url,hub.repository.clone());
    hub.deliveries.lock().unwrap().push(answer);
    fixture.bridge.flush_gateway().await.unwrap();
    assert_eq!(fixture.agent.answers.lock().unwrap().len(),1);
    assert!(fixture.sender.sent.lock().unwrap().is_empty());
    task.abort();
}

#[tokio::test]
async fn gateway_text_answer_rejects_malformed_or_mismatched_token_without_consuming_question() {
    let (fixture,hub,_,task) = gateway_fixture(false).await;
    fixture.bridge.flush_gateway().await.unwrap();
    let binding = fixture.bindings.get_by_conversation(&fixture.owner,"test_conversation").await.unwrap().unwrap();
    for request in ["first_question","second_question"] {
        fixture.bridge.publish_question(&binding,"message",&json!({"request_id":request,"questions":[{"question":request,"options":[{"label":"A"}]}]})).await.unwrap();
    }
    fixture.bridge.flush_interactions().await;
    let tokens: Vec<_> = fixture.bridge.interactions.lock().await.keys().cloned().collect();
    assert_eq!(tokens.len(),2);
    let route = hub.route.lock().unwrap().clone();
    let before = hub.acks.lock().unwrap().len();
    for (index,text) in [format!("/answer {} A",tokens[1]),format!("/answer {}   ",tokens[0]),
        format!("/answer {}",tokens[0]),format!("/answer\t{} A",tokens[0]),format!("/answer_wrong {} A",tokens[0])].iter().enumerate() {
        let mut answer = text_answer(&route,&format!("invalid_answer_{index}"),&tokens[0],text);
        assert!(fixture.bridge.accept_gateway_delivery(&answer).await.is_err());
        answer.event.as_mut().unwrap().platform_message_id = "new_user_message".into();
        assert!(fixture.bridge.accept_gateway_delivery(&answer).await.is_err());
    }
    assert_eq!(hub.acks.lock().unwrap().len(),before);
    assert_eq!(fixture.bridge.interactions.lock().await.len(),2);
    assert_eq!(hub.repository.list_interactions(now_ms()).await.unwrap().len(),2);
    assert!(hub.repository.pending_deliveries().await.unwrap().is_empty());
    assert!(fixture.agent.answers.lock().unwrap().is_empty());
    let mut card = click(&route,"card_without_message_id",&tokens[0]);
    card.event.as_mut().unwrap().platform_message_id.clear();
    assert!(fixture.bridge.accept_gateway_delivery(&card).await.is_err());
    task.abort();
}
