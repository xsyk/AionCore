use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use aionui_ai_agent::{
    AgentError, AgentInstance, AgentSendError, AgentStreamEvent, IAgentTask, IMockAgent, IWorkerTaskManager,
};
use aionui_api_types::{AskQuestionAnswer, WebSocketMessage};
use aionui_common::{
    AgentKillReason, AgentType, Confirmation, ConfirmationOption, ConversationStatus, TimestampMs, now_ms,
};
use aionui_conversation::ConversationService;
use aionui_conversation::skill_resolver::SkillResolver;
use aionui_db::models::{ConversationRow, FeishuLoginConfigRow};
use aionui_db::{
    IConversationRepository, IFeishuBindingRepository, IFeishuLoginRepository, IUserRepository,
    SqliteConversationRepository, SqliteFeishuBindingRepository, SqliteFeishuLoginRepository, SqliteUserRepository,
    UserStatus, init_database_memory,
};
use aionui_realtime::{BroadcastEventBus, EventBroadcaster};
use serde_json::{Value, json};
use tokio::sync::broadcast;

use super::{FeishuBridge, FeishuBridgeDependencies};
use crate::error::ChannelError;
use crate::stream_relay::ChannelSender;
use crate::types::{
    ActionCategory, ActionContext, MessageContentType, PluginType, UnifiedAction, UnifiedIncomingMessage,
    UnifiedMessageContent, UnifiedOutgoingMessage, UnifiedUser,
};

#[derive(Default)]
struct RecordingSender {
    sent: Mutex<Vec<UnifiedOutgoingMessage>>,
    edited: Mutex<Vec<UnifiedOutgoingMessage>>,
    attempts: Mutex<Vec<String>>,
    acknowledged: Mutex<HashMap<String, String>>,
    fail_before: AtomicUsize,
    fail_after: AtomicUsize,
    block_next: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl ChannelSender for RecordingSender {
    async fn send_message(
        &self,
        _: &str,
        _: &str,
        _: &str,
        message: UnifiedOutgoingMessage,
    ) -> Result<String, ChannelError> {
        let mut sent = self.sent.lock().unwrap();
        sent.push(message);
        Ok(format!("card_{}", sent.len()))
    }
    async fn send_message_idempotent(
        &self,
        _: &str,
        _: &str,
        _: &str,
        message: UnifiedOutgoingMessage,
        uuid: &str,
    ) -> Result<String, ChannelError> {
        self.attempts.lock().unwrap().push(uuid.to_owned());
        if self.block_next.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        if self
            .fail_before
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(ChannelError::MessageSendFailed("connection refused".into()));
        }
        let id = {
            let mut acknowledged = self.acknowledged.lock().unwrap();
            acknowledged
                .entry(uuid.to_owned())
                .or_insert_with(|| {
                    let mut sent = self.sent.lock().unwrap();
                    sent.push(message);
                    format!("remote_{}", sent.len())
                })
                .clone()
        };
        if self
            .fail_after
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(ChannelError::MessageSendFailed(
                "response lost after remote acceptance".into(),
            ));
        }
        Ok(id)
    }
    async fn edit_message(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
        message: UnifiedOutgoingMessage,
    ) -> Result<(), ChannelError> {
        self.edited.lock().unwrap().push(message);
        Ok(())
    }
}

type RecordedAnswers = Option<Vec<(String, Vec<String>)>>;

struct RecordingAgent {
    tx: broadcast::Sender<AgentStreamEvent>,
    confirmations: Mutex<Vec<Confirmation>>,
    decisions: Mutex<Vec<Value>>,
    answers: Mutex<Vec<RecordedAnswers>>,
}

#[async_trait::async_trait]
impl IAgentTask for RecordingAgent {
    fn agent_type(&self) -> AgentType {
        AgentType::Acp
    }
    fn conversation_id(&self) -> &str {
        "test_conversation"
    }
    fn workspace(&self) -> &str {
        "/tmp"
    }
    fn status(&self) -> Option<ConversationStatus> {
        None
    }
    fn last_activity_at(&self) -> TimestampMs {
        now_ms()
    }
    fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
        self.tx.subscribe()
    }
    async fn send_message(&self, _: aionui_ai_agent::types::SendMessageData) -> Result<(), AgentSendError> {
        Ok(())
    }
    async fn cancel(&self) -> Result<(), AgentError> {
        Ok(())
    }
    fn kill(&self, _: Option<AgentKillReason>) -> Result<(), AgentError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl IMockAgent for RecordingAgent {
    fn get_confirmations(&self) -> Vec<Confirmation> {
        self.confirmations.lock().unwrap().clone()
    }
    fn confirm(&self, _: &str, call: &str, value: Value, _: bool) -> Result<(), AgentError> {
        self.confirmations
            .lock()
            .unwrap()
            .retain(|pending| pending.call_id != call);
        self.decisions.lock().unwrap().push(value);
        Ok(())
    }
    fn answer_ask(&self, _: &str, answers: Option<Vec<AskQuestionAnswer>>) -> Result<(), AgentError> {
        self.answers.lock().unwrap().push(answers.map(|answers| {
            answers
                .into_iter()
                .map(|answer| (answer.question, answer.labels))
                .collect()
        }));
        Ok(())
    }
}

struct RecordingTasks(Arc<RecordingAgent>);

#[async_trait::async_trait]
impl IWorkerTaskManager for RecordingTasks {
    fn get_task(&self, id: &str) -> Option<AgentInstance> {
        (id == "test_conversation").then(|| AgentInstance::Mock(self.0.clone()))
    }
    async fn get_or_build_task(
        &self,
        _: &str,
        _: aionui_ai_agent::types::BuildTaskOptions,
    ) -> Result<AgentInstance, AgentError> {
        Ok(AgentInstance::Mock(self.0.clone()))
    }
    fn kill(&self, _: &str, _: Option<AgentKillReason>) -> Result<(), AgentError> {
        Ok(())
    }
    fn kill_and_wait(
        &self,
        _: &str,
        _: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(std::future::ready(()))
    }
    async fn clear(&self) {}
    fn active_count(&self) -> usize {
        1
    }
    fn collect_idle(&self, _: TimestampMs) -> Vec<String> {
        Vec::new()
    }
}

struct EmptySkills;

#[async_trait::async_trait]
impl SkillResolver for EmptySkills {
    async fn auto_inject_names(&self) -> Vec<String> {
        Vec::new()
    }
    async fn resolve_skills(&self, _: &[String]) -> Vec<aionui_extension::ResolvedAgentSkill> {
        Vec::new()
    }
}

struct Fixture {
    _database: aionui_db::Database,
    bridge: Arc<FeishuBridge>,
    bindings: Arc<SqliteFeishuBindingRepository>,
    owner: String,
    other: String,
    sender: Arc<RecordingSender>,
    agent: Arc<RecordingAgent>,
}

async fn fixture() -> Fixture {
    let db = init_database_memory().await.unwrap();
    let users = Arc::new(SqliteUserRepository::new(db.pool().clone()));
    let owner = users
        .create_external_local_user("feishu:on_owner", "owner", "hash", UserStatus::Active)
        .await
        .unwrap()
        .id;
    let other = users
        .create_external_local_user("feishu:on_other", "other", "hash", UserStatus::Active)
        .await
        .unwrap()
        .id;
    let repo = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
    repo.create(&ConversationRow {
        id: "test_conversation".into(),
        user_id: owner.clone(),
        name: "Existing history".into(),
        r#type: "acp".into(),
        extra: "{}".into(),
        model: None,
        status: Some("pending".into()),
        source: Some("aionui".into()),
        channel_chat_id: None,
        pinned: false,
        pinned_at: None,
        created_at: 1,
        updated_at: 1,
        project_id: None,
        folder_id: None,
        name_source: None,
    })
    .await
    .unwrap();
    let login = Arc::new(SqliteFeishuLoginRepository::new(db.pool().clone()));
    login
        .save(&FeishuLoginConfigRow {
            enabled: true,
            app_id: "app".into(),
            tenant_key: Some("tenant".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let (tx, _) = broadcast::channel(16);
    let agent = Arc::new(RecordingAgent {
        tx,
        confirmations: Mutex::new(Vec::new()),
        decisions: Mutex::new(Vec::new()),
        answers: Mutex::new(Vec::new()),
    });
    let tasks: Arc<dyn IWorkerTaskManager> = Arc::new(RecordingTasks(agent.clone()));
    let bus: Arc<dyn EventBroadcaster> = Arc::new(BroadcastEventBus::new(32));
    let conversations = Arc::new(ConversationService::new(
        std::env::temp_dir(),
        bus,
        Arc::new(EmptySkills),
        tasks.clone(),
        repo.clone(),
        Arc::new(aionui_db::SqliteAgentMetadataRepository::new(db.pool().clone())),
        Arc::new(aionui_db::SqliteAcpSessionRepository::new(db.pool().clone())),
    ));
    let bindings = Arc::new(SqliteFeishuBindingRepository::new(db.pool().clone()));
    let sender = Arc::new(RecordingSender::default());
    let bridge = Arc::new(FeishuBridge::new(FeishuBridgeDependencies {
        bindings: bindings.clone(),
        conversations,
        conversation_repo: repo,
        users,
        login_config: login,
        tasks,
        sender: sender.clone(),
    }));
    Fixture {
        _database: db,
        bridge,
        bindings,
        owner,
        other,
        sender,
        agent,
    }
}

fn incoming(fixture: &Fixture, union: &str, text: &str, chat: &str) -> UnifiedIncomingMessage {
    UnifiedIncomingMessage {
        owner_user_id: Some(fixture.owner.clone()),
        id: aionui_common::generate_id(),
        platform: PluginType::Lark,
        chat_id: chat.into(),
        user: UnifiedUser {
            id: format!("ou_{union}"),
            username: None,
            display_name: union.into(),
            avatar_url: None,
        },
        content: UnifiedMessageContent {
            content_type: MessageContentType::Text,
            text: text.into(),
            attachments: None,
        },
        timestamp: now_ms(),
        reply_to_message_id: None,
        action: None,
        raw: Some(
            json!({"feishu_transport":{"app_id":"app","bot_open_id":"bot"},"feishu_event":{"app_id":"app","tenant_key":"tenant","union_id":union,"chat_type":"group","sender_type":"user","mentioned_ids":["bot"]}}),
        ),
    }
}

async fn bind(fixture: &Fixture) -> aionui_db::models::FeishuBindingRow {
    let code = fixture
        .bridge
        .create_binding_code(&fixture.owner, "test_conversation")
        .await
        .unwrap();
    assert!(
        fixture
            .bridge
            .handle_incoming(&incoming(
                fixture,
                "on_owner",
                &format!("/bind {}", code.bind_code),
                "group"
            ))
            .await
    );
    fixture
        .bindings
        .get_by_conversation(&fixture.owner, "test_conversation")
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn only_owner_can_generate_or_remove_binding_and_group_member_cannot_redeem_owner_code() {
    let fixture = fixture().await;
    assert!(matches!(
        fixture
            .bridge
            .create_binding_code(&fixture.other, "test_conversation")
            .await,
        Err(ChannelError::SessionNotFound(_)) | Err(ChannelError::UserNotAuthorized(_))
    ));
    let code = fixture
        .bridge
        .create_binding_code(&fixture.owner, "test_conversation")
        .await
        .unwrap();
    fixture
        .bridge
        .handle_incoming(&incoming(
            &fixture,
            "on_other",
            &format!("/bind {}", code.bind_code),
            "group",
        ))
        .await;
    assert!(
        fixture
            .bindings
            .get_by_conversation(&fixture.owner, "test_conversation")
            .await
            .unwrap()
            .is_none()
    );
    fixture
        .bridge
        .handle_incoming(&incoming(
            &fixture,
            "on_owner",
            &format!("/bind {}", code.bind_code),
            "group",
        ))
        .await;
    assert!(
        fixture
            .bridge
            .unbind(&fixture.other, "test_conversation")
            .await
            .is_err()
    );
    assert!(
        fixture
            .bindings
            .get_by_conversation(&fixture.owner, "test_conversation")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn team_owned_conversation_is_rejected_before_issuing_binding_code() {
    let fixture = fixture().await;
    fixture
        .bridge
        .deps
        .conversation_repo
        .update(
            &fixture.owner,
            "test_conversation",
            &aionui_db::ConversationRowUpdate {
                extra: Some(json!({"teamId":"team_1"}).to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let error = fixture
        .bridge
        .create_binding_code(&fixture.owner, "test_conversation")
        .await
        .unwrap_err();
    assert!(matches!(&error, ChannelError::UnsupportedConversation(_)));
    assert!(error.to_string().contains("FEISHU_BINDING_UNSUPPORTED"));
    assert!(
        fixture
            .bindings
            .get_by_conversation(&fixture.owner, "test_conversation")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn group_member_can_answer_permission_once_but_forwarded_or_stale_card_cannot() {
    let fixture = fixture().await;
    let binding = bind(&fixture).await;
    let value = json!({"option_id":"provider_opaque_allow","extra":7});
    let confirmation = Confirmation {
        id: "permission".into(),
        call_id: "permission".into(),
        title: Some("Execute".into()),
        action: None,
        description: "Approval required".into(),
        command_type: None,
        questions: None,
        options: vec![ConfirmationOption {
            label: "Allow".into(),
            value: value.clone(),
            params: None,
        }],
    };
    fixture.agent.confirmations.lock().unwrap().push(confirmation.clone());
    fixture
        .bridge
        .publish_confirmation(&binding, "assistant_msg", confirmation)
        .await
        .unwrap();
    fixture.bridge.flush_interactions().await;
    let button = fixture
        .sender
        .sent
        .lock()
        .unwrap()
        .iter()
        .find_map(|message| message.buttons.as_ref())
        .unwrap()[0][0]
        .clone();
    let mut click = incoming(&fixture, "on_other", "", "another_group");
    click.action = Some(UnifiedAction {
        action: button.action.clone(),
        category: ActionCategory::System,
        params: button.params.clone(),
        context: ActionContext {
            platform: PluginType::Lark,
            user_id: click.user.id.clone(),
            chat_id: click.chat_id.clone(),
            message_id: Some("card".into()),
            session_id: None,
        },
    });
    fixture.bridge.handle_incoming(&click).await;
    assert!(fixture.agent.decisions.lock().unwrap().is_empty());
    click.chat_id = "group".into();
    click.action.as_mut().unwrap().context.chat_id = "group".into();
    fixture.bridge.handle_incoming(&click).await;
    fixture.bridge.handle_incoming(&click).await;
    assert_eq!(*fixture.agent.decisions.lock().unwrap(), vec![value]);
    fixture
        .bridge
        .unbind(&fixture.owner, "test_conversation")
        .await
        .unwrap();
    fixture.bridge.handle_incoming(&click).await;
    assert_eq!(fixture.agent.decisions.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn group_member_answers_multiple_questions_over_dedicated_ask_channel() {
    let fixture = fixture().await;
    let binding = bind(&fixture).await;
    fixture.bridge.publish_question(&binding, "assistant_msg", &json!({"request_id":"ask_1","questions":[{"question":"Targets","multi_select":true,"options":[{"label":"a"},{"label":"b"}]},{"question":"Notes","options":[]}]})).await.unwrap();
    fixture.bridge.flush_interactions().await;
    let token = fixture.bridge.interactions.lock().await.keys().next().unwrap().clone();
    let answer = incoming(&fixture, "on_other", &format!("/answer {token} a,b | ready"), "group");
    fixture.bridge.handle_incoming(&answer).await;
    fixture.bridge.handle_incoming(&answer).await;
    assert!(fixture.agent.decisions.lock().unwrap().is_empty());
    assert_eq!(
        *fixture.agent.answers.lock().unwrap(),
        vec![Some(vec![
            ("Targets".into(), vec!["a".into(), "b".into()]),
            ("Notes".into(), vec!["ready".into()])
        ])]
    );
}

async fn persist_text(fixture: &Fixture, id: &str, content: &str, is_user: bool, hidden: bool, status: &str) {
    fixture
        .bridge
        .deps
        .conversation_repo
        .insert_message(
            &fixture.owner,
            &aionui_db::models::MessageRow {
                id: id.into(),
                conversation_id: "test_conversation".into(),
                msg_id: Some(id.into()),
                r#type: "text".into(),
                content: json!({"content":content,"__aion_finalized":!is_user && status=="finish"}).to_string(),
                position: Some(if is_user { "right" } else { "left" }.into()),
                status: Some(status.into()),
                hidden,
                created_at: now_ms(),
                backend_turn_id: None,
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn web_messages_and_agent_reply_sync_but_history_hidden_and_incoming_echo_do_not() {
    let fixture = fixture().await;
    // Deliberately use the same millisecond: binding history is an atomic ID snapshot.
    persist_text(&fixture, "old", "old history", true, false, "finish").await;
    let binding = bind(&fixture).await;
    fixture.sender.sent.lock().unwrap().clear();
    persist_text(&fixture, "new", "web prompt", true, false, "finish").await;
    persist_text(&fixture, "hidden", "internal", true, true, "finish").await;
    persist_text(&fixture, "feishu:app:input", "from IM", true, false, "finish").await;
    persist_text(&fixture, "reply", "Hello world", false, false, "finish").await;
    // Old created_at is irrelevant for a genuinely new row (clock skew/rebinding).
    sqlx::query("UPDATE messages SET created_at=? WHERE id='new'")
        .bind(binding.bound_at - 1000)
        .execute(fixture._database.pool())
        .await
        .unwrap();
    fixture.bridge.flush_deliveries().await.unwrap();
    fixture.bridge.flush_deliveries().await.unwrap();
    let sent = fixture.sender.sent.lock().unwrap();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].text.as_deref(), Some("网页消息\nweb prompt"));
    assert_eq!(sent[1].text.as_deref(), Some("Hello world"));
}

#[tokio::test]
async fn outbound_failure_before_send_and_lost_ack_retry_same_durable_uuid() {
    let fixture = fixture().await;
    bind(&fixture).await;
    fixture.sender.sent.lock().unwrap().clear();
    persist_text(&fixture, "long_reply", &"界".repeat(7000), false, false, "finish").await;
    fixture.sender.fail_before.store(1, Ordering::SeqCst);
    fixture.bridge.flush_deliveries().await.unwrap();
    assert_eq!(fixture.sender.sent.lock().unwrap().len(), 0);
    assert_eq!(fixture.bridge.deps.bindings.pending_outbox().await.unwrap().len(), 3);
    fixture.sender.fail_after.store(1, Ordering::SeqCst);
    fixture.bridge.flush_deliveries().await.unwrap();
    assert_eq!(fixture.sender.sent.lock().unwrap().len(), 1);
    fixture.bridge.flush_deliveries().await.unwrap();
    assert_eq!(fixture.sender.sent.lock().unwrap().len(), 3);
    {
        let attempts = fixture.sender.attempts.lock().unwrap();
        assert_eq!(attempts[0], attempts[1]);
        assert_eq!(attempts[1], attempts[2]);
    }
    assert!(fixture.bridge.deps.bindings.pending_outbox().await.unwrap().is_empty());
}

#[tokio::test]
async fn inbound_retries_before_persist_and_after_persist_do_not_lose_or_duplicate_input() {
    let fixture = fixture().await;
    bind(&fixture).await;
    let message = incoming(&fixture, "on_other", "group input", "group");
    let origin = format!("feishu:app:{}", message.id);
    sqlx::query("CREATE TRIGGER reject_transport BEFORE INSERT ON messages WHEN NEW.id LIKE 'feishu:%' BEGIN SELECT RAISE(FAIL,'temporary write failure'); END").execute(fixture._database.pool()).await.unwrap();
    fixture.bridge.handle_incoming(&message).await;
    fixture.bridge.flush_inbox().await.unwrap();
    assert_eq!(fixture.bindings.pending_inbox().await.unwrap().len(), 1);
    assert!(
        fixture
            .bridge
            .deps
            .conversation_repo
            .get_message(&fixture.owner, "test_conversation", &origin)
            .await
            .unwrap()
            .is_none()
    );
    sqlx::query("DROP TRIGGER reject_transport")
        .execute(fixture._database.pool())
        .await
        .unwrap();
    // Inject the real boundary: message persisted, inbox checkpoint write fails.
    sqlx::query("CREATE TRIGGER reject_checkpoint BEFORE UPDATE ON feishu_inbox WHEN NEW.done=1 BEGIN SELECT RAISE(FAIL,'checkpoint failure'); END").execute(fixture._database.pool()).await.unwrap();
    assert!(fixture.bridge.flush_inbox().await.is_err());
    let accepted = fixture
        .bridge
        .deps
        .conversation_repo
        .get_message(&fixture.owner, "test_conversation", &origin)
        .await
        .unwrap()
        .unwrap();
    assert!(accepted.content.contains("on_other"));
    assert!(accepted.content.contains("group input"));
    sqlx::query("DROP TRIGGER reject_checkpoint")
        .execute(fixture._database.pool())
        .await
        .unwrap();
    fixture.bridge.handle_incoming(&message).await;
    fixture.bridge.flush_inbox().await.unwrap();
    assert!(fixture.bindings.pending_inbox().await.unwrap().is_empty());
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE id=?")
        .bind(&origin)
        .fetch_one(fixture._database.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn hidden_replace_and_multi_part_shortening_wait_for_final_persistence() {
    let fixture = fixture().await;
    bind(&fixture).await;
    fixture.sender.sent.lock().unwrap().clear();
    persist_text(&fixture, "rewrite", &"provisional".repeat(900), false, false, "pending").await;
    persist_text(&fixture, "withdraw", "internal protocol text", false, false, "pending").await;
    fixture.bridge.flush_deliveries().await.unwrap();
    assert!(fixture.sender.sent.lock().unwrap().is_empty());
    for (id, content, hidden) in [("rewrite", "short final", false), ("withdraw", "", true)] {
        fixture.bridge.handle_event(WebSocketMessage::new("message.stream", json!({"conversation_id":"test_conversation","user_id":fixture.owner,"msg_id":id,"type":"finish","data":{}}))).await.unwrap();
        fixture.bridge.handle_event(WebSocketMessage::new("message.stream", json!({"conversation_id":"test_conversation","user_id":fixture.owner,"msg_id":id,"type":"content","data":{"content":content},"replace":true,"hidden":hidden}))).await.unwrap();
        fixture
            .bridge
            .deps
            .conversation_repo
            .update_message(
                &fixture.owner,
                "test_conversation",
                id,
                &aionui_db::MessageRowUpdate {
                    content: Some(json!({"content":content,"__aion_finalized":true}).to_string()),
                    status: Some(Some("finish".into())),
                    hidden: Some(hidden),
                },
            )
            .await
            .unwrap();
    }
    fixture
        .bridge
        .handle_event(WebSocketMessage::new(
            "turn.completed",
            json!({"conversation_id":"test_conversation","user_id":fixture.owner}),
        ))
        .await
        .unwrap();
    fixture.bridge.flush_deliveries().await.unwrap();
    fixture.bridge.flush_deliveries().await.unwrap();
    let sent = fixture.sender.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].text.as_deref(), Some("short final"));
    assert!(fixture.sender.edited.lock().unwrap().is_empty());
}

#[tokio::test]
async fn slow_remote_send_does_not_block_long_connection_events_and_text_recovers_without_events() {
    let fixture = fixture().await;
    let binding = bind(&fixture).await;
    persist_text(
        &fixture,
        "recover_without_event",
        "durable final",
        false,
        false,
        "finish",
    )
    .await;
    fixture
        .bridge
        .publish_question(
            &binding,
            "ask_message",
            &json!({"request_id":"ask_slow","questions":[{"question":"Q","options":[]}]}),
        )
        .await
        .unwrap();
    fixture.sender.block_next.store(true, Ordering::SeqCst);
    let (events, rx) = broadcast::channel(512);
    let task = tokio::spawn(fixture.bridge.clone().run(rx));
    tokio::time::timeout(Duration::from_secs(2), fixture.sender.entered.notified())
        .await
        .unwrap();
    for n in 0..200 {
        events.send(WebSocketMessage::new("message.stream", json!({"conversation_id":"test_conversation","user_id":fixture.owner,"msg_id":format!("chunk_{n}"),"type":"content","data":{"content":"x"}}))).unwrap();
    }
    events
        .send(WebSocketMessage::new(
            "confirmation.remove",
            json!({"conversation_id":"test_conversation","user_id":fixture.owner,"id":"ask_slow"}),
        ))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if fixture.bridge.interactions.lock().await.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    fixture.sender.release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if fixture
                .sender
                .sent
                .lock()
                .unwrap()
                .iter()
                .any(|message| message.text.as_deref() == Some("durable final"))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn unbind_cancels_pending_inbox_and_outbox() {
    let fixture = fixture().await;
    bind(&fixture).await;
    persist_text(&fixture, "unsent", "waiting", true, false, "finish").await;
    fixture.sender.fail_before.store(1, Ordering::SeqCst);
    fixture.bridge.flush_deliveries().await.unwrap();
    fixture
        .bridge
        .handle_incoming(&incoming(&fixture, "on_other", "not yet processed", "group"))
        .await;
    assert!(!fixture.bindings.pending_inbox().await.unwrap().is_empty());
    assert!(!fixture.bindings.pending_outbox().await.unwrap().is_empty());
    fixture
        .bridge
        .unbind(&fixture.owner, "test_conversation")
        .await
        .unwrap();
    assert!(fixture.bindings.pending_inbox().await.unwrap().is_empty());
    assert!(fixture.bindings.pending_outbox().await.unwrap().is_empty());
}

#[tokio::test]
async fn finished_text_segments_are_held_while_the_real_turn_claim_is_active() {
    let fixture = fixture().await;
    bind(&fixture).await;
    fixture.sender.sent.lock().unwrap().clear();
    // Use the same authoritative claim object held by TurnOrchestrator.
    let runtime = fixture.bridge.deps.conversations.runtime_state();
    let claim = runtime.try_claim_turn("test_conversation", "test_turn").unwrap();
    assert!(
        fixture
            .bridge
            .deps
            .conversations
            .runtime_summary_for("test_conversation")
            .await
            .turn_id
            .is_some()
    );
    persist_text(
        &fixture,
        "closed_segment",
        &"provisional".repeat(1000),
        false,
        false,
        "finish",
    )
    .await;
    fixture.bridge.flush_deliveries().await.unwrap();
    assert!(fixture.sender.sent.lock().unwrap().is_empty());
    fixture
        .bridge
        .deps
        .conversation_repo
        .update_message(
            &fixture.owner,
            "test_conversation",
            "closed_segment",
            &aionui_db::MessageRowUpdate {
                content: Some(json!({"content":"final replacement","__aion_finalized":true}).to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    drop(claim);
    assert!(
        fixture
            .bridge
            .deps
            .conversations
            .runtime_summary_for("test_conversation")
            .await
            .turn_id
            .is_none()
    );
    fixture.bridge.flush_deliveries().await.unwrap();
    let sent = fixture.sender.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].text.as_deref(), Some("final replacement"));
}

#[tokio::test]
async fn unknown_remote_outcome_beyond_provider_dedup_window_is_retained_without_blind_resend() {
    let fixture = fixture().await;
    bind(&fixture).await;
    persist_text(&fixture, "uncertain", "retry safely", false, false, "finish").await;
    fixture.sender.fail_after.store(1, Ordering::SeqCst);
    fixture.bridge.flush_deliveries().await.unwrap();
    let pending = fixture.bindings.pending_outbox().await.unwrap();
    assert_eq!(pending.len(), 1);
    sqlx::query("UPDATE feishu_outbox SET first_attempt_at=? WHERE id=?")
        .bind(now_ms() - 60 * 60 * 1000)
        .bind(&pending[0].id)
        .execute(fixture._database.pool())
        .await
        .unwrap();
    let attempts = fixture.sender.attempts.lock().unwrap().len();
    fixture.bridge.flush_deliveries().await.unwrap();
    assert_eq!(fixture.sender.attempts.lock().unwrap().len(), attempts);
    assert!(fixture.bindings.pending_outbox().await.unwrap().is_empty());
    let unknown: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM feishu_outbox WHERE delivery_state='unknown'")
        .fetch_one(fixture._database.pool())
        .await
        .unwrap();
    assert_eq!(unknown, 1);
}

#[tokio::test]
async fn unclaimed_background_segments_require_terminal_middleware_finalization_marker() {
    let fixture = fixture().await;
    bind(&fixture).await;
    fixture.sender.sent.lock().unwrap().clear();
    persist_text(
        &fixture,
        "orphan_segment",
        "not yet processed by terminal middleware",
        false,
        false,
        "work",
    )
    .await;
    fixture
        .bridge
        .deps
        .conversation_repo
        .update_message(
            &fixture.owner,
            "test_conversation",
            "orphan_segment",
            &aionui_db::MessageRowUpdate {
                status: Some(Some("finish".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        fixture
            .bridge
            .deps
            .conversations
            .runtime_summary_for("test_conversation")
            .await
            .turn_id
            .is_none()
    );
    fixture.bridge.flush_deliveries().await.unwrap();
    assert!(fixture.sender.sent.lock().unwrap().is_empty());
    fixture
        .bridge
        .deps
        .conversation_repo
        .update_message(
            &fixture.owner,
            "test_conversation",
            "orphan_segment",
            &aionui_db::MessageRowUpdate {
                content: Some(json!({"content":"background final","__aion_finalized":true}).to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    fixture.bridge.flush_deliveries().await.unwrap();
    let sent = fixture.sender.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].text.as_deref(), Some("background final"));
}

#[cfg(feature = "lark")]
#[tokio::test]
async fn production_frame_acceptance_commits_before_success_ack_and_failed_fragment_can_redeliver() {
    use crate::plugin::DurableChannelIngress;
    use crate::plugins::lark::{
        FragmentCache, LarkIngressTransport, METHOD_DATA, PbFrame, PbHeader, accept_data_frame,
    };
    let fixture = fixture().await;
    bind(&fixture).await;
    let (messages, _message_rx) = tokio::sync::mpsc::channel(16);
    let (confirms, _confirm_rx) = tokio::sync::mpsc::channel(16);
    let manager = crate::manager::ChannelManager::new(
        Arc::new(aionui_db::SqliteChannelRepository::new(
            fixture._database.pool().clone(),
        )),
        Arc::new(BroadcastEventBus::new(16)),
        [0; 32],
        messages.clone(),
        confirms.clone(),
    );
    let ingress: Arc<dyn DurableChannelIngress> = fixture.bridge.clone();
    manager.set_durable_ingress(Arc::downgrade(&ingress));
    let callbacks = manager.callbacks_for_owner(&fixture.owner);
    let transport = LarkIngressTransport {
        app_id: "app".into(),
        bot_open_id: "bot".into(),
        ingress: callbacks.durable_ingress,
    };
    let payload = json!({"schema":"2.0","header":{"event_id":"durable_event","event_type":"im.message.receive_v1","app_id":"app"},"event":{"sender":{"sender_id":{"open_id":"ou_member","union_id":"on_other","user_id":"u"},"sender_type":"user","tenant_key":"tenant"},"message":{"message_id":"packet_input","chat_id":"group","chat_type":"group","message_type":"text","content":"{\"text\":\"durable packet\"}","mentions":[{"key":"@_user_1","id":{"open_id":"bot"},"name":"Bot","tenant_key":"tenant"}],"create_time":"1000"}}}).to_string().into_bytes();
    let midpoint = payload.len() / 2;
    let frame = |seq: usize, bytes: &[u8]| PbFrame {
        method: METHOD_DATA,
        headers: vec![
            PbHeader {
                key: "message_id".into(),
                value: "packet_frame".into(),
            },
            PbHeader {
                key: "type".into(),
                value: "event".into(),
            },
            PbHeader {
                key: "sum".into(),
                value: "2".into(),
            },
            PbHeader {
                key: "seq".into(),
                value: seq.to_string(),
            },
        ],
        payload: bytes.to_vec(),
        ..Default::default()
    };
    let first = frame(0, &payload[..midpoint]);
    let last = frame(1, &payload[midpoint..]);
    let mut fragments = FragmentCache::new();
    let dedup = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    assert!(
        accept_data_frame(&first, &mut fragments, &transport, &messages, &confirms, &dedup)
            .await
            .unwrap()
            .is_none()
    );
    assert!(fixture.bindings.pending_ingress().await.unwrap().is_empty());
    sqlx::query("CREATE TRIGGER reject_raw BEFORE INSERT ON feishu_ingress_events BEGIN SELECT RAISE(FAIL,'ingress unavailable'); END").execute(fixture._database.pool()).await.unwrap();
    // The same function used by the WebSocket loop cannot return a 200 ACK.
    assert!(
        accept_data_frame(&last, &mut fragments, &transport, &messages, &confirms, &dedup)
            .await
            .is_err()
    );
    assert!(fixture.bindings.pending_ingress().await.unwrap().is_empty());
    assert!(dedup.lock().await.is_empty());
    sqlx::query("DROP TRIGGER reject_raw")
        .execute(fixture._database.pool())
        .await
        .unwrap();
    // Last-fragment retry uses retained reassembly state and real owner adapter.
    let ack = accept_data_frame(&last, &mut fragments, &transport, &messages, &confirms, &dedup)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&ack.payload).unwrap()["code"], 200);
    let accepted = fixture.bindings.pending_ingress().await.unwrap();
    assert_eq!(accepted.len(), 1);
    let receipt: Value = serde_json::from_str(&accepted[0].payload).unwrap();
    let raw: crate::plugin::DurableIngressEvent = serde_json::from_value(receipt["event"].clone()).unwrap();
    assert_eq!(raw.owner_user_id, fixture.owner);
    assert!(fixture.bindings.pending_inbox().await.unwrap().is_empty());
    fixture.bridge.flush_deliveries().await.unwrap();
    let origin = "feishu:app:packet_input";
    let stored = fixture
        .bridge
        .deps
        .conversation_repo
        .get_message(&fixture.owner, "test_conversation", origin)
        .await
        .unwrap()
        .unwrap();
    assert!(stored.content.contains("durable packet"));
    accept_data_frame(&last, &mut fragments, &transport, &messages, &confirms, &dedup)
        .await
        .unwrap()
        .unwrap();
    fixture.bridge.flush_deliveries().await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE id=?")
        .bind(origin)
        .fetch_one(fixture._database.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
    assert!(fixture.bindings.pending_ingress().await.unwrap().is_empty());
}

#[tokio::test]
async fn one_hundred_unknown_deliveries_are_parked_without_starving_another_chat() {
    use aionui_db::models::FeishuOutboxRow;
    let fixture = fixture().await;
    let binding = bind(&fixture).await;
    let mut second = fixture
        .bridge
        .deps
        .conversation_repo
        .get(&fixture.owner, "test_conversation")
        .await
        .unwrap()
        .unwrap();
    second.id = "second_conversation".into();
    fixture.bridge.deps.conversation_repo.create(&second).await.unwrap();
    let code = fixture
        .bridge
        .create_binding_code(&fixture.owner, &second.id)
        .await
        .unwrap();
    fixture
        .bridge
        .handle_incoming(&incoming(
            &fixture,
            "on_owner",
            &format!("/bind {}", code.bind_code),
            "second_group",
        ))
        .await;
    fixture.sender.sent.lock().unwrap().clear();
    let rows: Vec<_> = (0..100)
        .map(|part| FeishuOutboxRow {
            id: format!("unknown_{part}"),
            binding_id: binding.id.clone(),
            message_id: format!("uncertain_{part}"),
            part: 0,
            content: "uncertain".into(),
            platform_message_id: None,
            first_attempt_at: None,
        })
        .collect();
    fixture.bindings.enqueue_outbox(&rows).await.unwrap();
    sqlx::query("UPDATE feishu_outbox SET first_attempt_at=?")
        .bind(now_ms() - 60 * 60 * 1000)
        .execute(fixture._database.pool())
        .await
        .unwrap();
    fixture
        .bridge
        .deps
        .conversation_repo
        .insert_message(
            &fixture.owner,
            &aionui_db::models::MessageRow {
                id: "fresh_other_chat".into(),
                conversation_id: second.id,
                msg_id: None,
                r#type: "text".into(),
                content: json!({"content":"fresh for other chat"}).to_string(),
                position: Some("right".into()),
                status: Some("finish".into()),
                hidden: false,
                created_at: now_ms(),
                backend_turn_id: None,
            },
        )
        .await
        .unwrap();
    fixture.bridge.flush_deliveries().await.unwrap();
    let unknown: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM feishu_outbox WHERE delivery_state='unknown'")
        .fetch_one(fixture._database.pool())
        .await
        .unwrap();
    assert_eq!(unknown, 100);
    assert!(fixture.bindings.pending_outbox().await.unwrap().is_empty());
    let sent = fixture.sender.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].text.as_deref(), Some("网页消息\nfresh for other chat"));
}

#[tokio::test]
async fn switching_binding_waits_for_an_inflight_send_before_committing() {
    let fixture = fixture().await;
    let original = bind(&fixture).await;
    persist_text(&fixture, "inflight", "deliver before switch", true, false, "finish").await;
    let code = fixture
        .bridge
        .create_binding_code(&fixture.owner, "test_conversation")
        .await
        .unwrap();
    fixture.sender.block_next.store(true, Ordering::SeqCst);
    let worker = tokio::spawn({
        let bridge = fixture.bridge.clone();
        async move { bridge.flush_deliveries().await }
    });
    tokio::time::timeout(Duration::from_secs(2), fixture.sender.entered.notified())
        .await
        .unwrap();
    let command = incoming(&fixture, "on_owner", &format!("/bind {}", code.bind_code), "new_group");
    let mut switch = tokio::spawn({
        let bridge = fixture.bridge.clone();
        async move { bridge.handle_incoming(&command).await }
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut switch)
            .await
            .is_err()
    );
    assert_eq!(
        fixture
            .bindings
            .get_by_conversation(&fixture.owner, "test_conversation")
            .await
            .unwrap()
            .unwrap()
            .id,
        original.id
    );
    fixture.sender.release.notify_one();
    worker.await.unwrap().unwrap();
    switch.await.unwrap();
    let current = fixture
        .bindings
        .get_by_conversation(&fixture.owner, "test_conversation")
        .await
        .unwrap()
        .unwrap();
    assert_ne!(current.id, original.id);
    assert_eq!(current.chat_id, "new_group");
}

#[tokio::test]
async fn queued_old_chat_input_and_approval_are_rechecked_after_binding_switch() {
    let fixture = fixture().await;
    let binding = bind(&fixture).await;
    let confirmation = Confirmation {
        id: "race_permission".into(),
        call_id: "race_permission".into(),
        title: None,
        action: None,
        description: "confirm".into(),
        command_type: None,
        questions: None,
        options: vec![ConfirmationOption {
            label: "Allow".into(),
            value: json!({"allow":true}),
            params: None,
        }],
    };
    fixture.agent.confirmations.lock().unwrap().push(confirmation.clone());
    fixture
        .bridge
        .publish_confirmation(&binding, "request", confirmation)
        .await
        .unwrap();
    fixture.bridge.flush_interactions().await;
    let button = fixture
        .sender
        .sent
        .lock()
        .unwrap()
        .iter()
        .find_map(|message| message.buttons.as_ref())
        .unwrap()[0][0]
        .clone();
    let lock = fixture
        .bridge
        .conversation_locks
        .entry("test_conversation".into())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone();
    let guard = lock.lock().await;
    let code = fixture
        .bridge
        .create_binding_code(&fixture.owner, "test_conversation")
        .await
        .unwrap();
    let command = incoming(&fixture, "on_owner", &format!("/bind {}", code.bind_code), "new_group");
    let mut switching = tokio::spawn({
        let bridge = fixture.bridge.clone();
        async move { bridge.handle_incoming(&command).await }
    });
    // Management lock proves switching has selected its old/new set and is now
    // waiting on the conversation guard, ahead of these old-chat operations.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if fixture.bridge.binding_management_lock.try_lock().is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut switching)
            .await
            .is_err()
    );
    let input = incoming(&fixture, "on_other", "must not reach replacement session", "group");
    let sending = tokio::spawn({
        let bridge = fixture.bridge.clone();
        async move { bridge.handle_incoming(&input).await }
    });
    let mut click = incoming(&fixture, "on_other", "", "group");
    click.action = Some(UnifiedAction {
        action: button.action,
        category: ActionCategory::System,
        params: button.params,
        context: ActionContext {
            platform: PluginType::Lark,
            user_id: click.user.id.clone(),
            chat_id: "group".into(),
            message_id: Some("card".into()),
            session_id: None,
        },
    });
    let approving = tokio::spawn({
        let bridge = fixture.bridge.clone();
        async move { bridge.handle_incoming(&click).await }
    });
    // Both operations may be before or after the final DB switch; all valid
    // schedules must reject their stale generation or cancel the old queue.
    tokio::task::yield_now().await;
    drop(guard);
    switching.await.unwrap();
    sending.await.unwrap();
    approving.await.unwrap();
    assert!(fixture.agent.decisions.lock().unwrap().is_empty());
    assert!(fixture.bindings.pending_inbox().await.unwrap().is_empty());
    assert_eq!(
        fixture
            .bindings
            .get_by_conversation(&fixture.owner, "test_conversation")
            .await
            .unwrap()
            .unwrap()
            .chat_id,
        "new_group"
    );
}

#[tokio::test]
async fn previously_persisted_input_after_restart_surfaces_uncertainty_without_executing_again() {
    let fixture = fixture().await;
    let binding = bind(&fixture).await;
    fixture.sender.sent.lock().unwrap().clear();
    let origin = "feishu:app:crash_before_dispatch";
    fixture
        .bindings
        .enqueue_inbox(origin, &binding.id, "saved before process exit")
        .await
        .unwrap();
    fixture
        .bindings
        .start_inbox_attempt(origin, "previous_process")
        .await
        .unwrap();
    persist_text(&fixture, origin, "saved before process exit", true, false, "finish").await;
    fixture.bridge.flush_deliveries().await.unwrap();
    assert!(fixture.bindings.pending_inbox().await.unwrap().is_empty());
    let tip = fixture
        .bridge
        .deps
        .conversation_repo
        .get_message(&fixture.owner, "test_conversation", &format!("{origin}:interrupted"))
        .await
        .unwrap()
        .unwrap();
    assert!(tip.content.contains("无法确认是否交给 AI 执行"));
    assert!(fixture.sender.sent.lock().unwrap().iter().any(|message| {
        message
            .text
            .as_deref()
            .is_some_and(|text| text.contains("无法确认是否交给 AI 执行"))
    }));
    fixture.bridge.flush_deliveries().await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE id=?")
        .bind(origin)
        .fetch_one(fixture._database.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn concurrent_cross_conversation_switches_acquire_locks_in_stable_order() {
    let fixture = fixture().await;
    bind(&fixture).await;
    let mut second = fixture
        .bridge
        .deps
        .conversation_repo
        .get(&fixture.owner, "test_conversation")
        .await
        .unwrap()
        .unwrap();
    second.id = "second_conversation".into();
    fixture.bridge.deps.conversation_repo.create(&second).await.unwrap();
    let first_code = fixture
        .bridge
        .create_binding_code(&fixture.owner, &second.id)
        .await
        .unwrap();
    fixture
        .bridge
        .handle_incoming(&incoming(
            &fixture,
            "on_owner",
            &format!("/bind {}", first_code.bind_code),
            "second_group",
        ))
        .await;
    let move_second = fixture
        .bridge
        .create_binding_code(&fixture.owner, &second.id)
        .await
        .unwrap();
    let move_first = fixture
        .bridge
        .create_binding_code(&fixture.owner, "test_conversation")
        .await
        .unwrap();
    let first = incoming(
        &fixture,
        "on_owner",
        &format!("/bind {}", move_second.bind_code),
        "group",
    );
    let second = incoming(
        &fixture,
        "on_owner",
        &format!("/bind {}", move_first.bind_code),
        "second_group",
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(
            fixture.bridge.handle_incoming(&first),
            fixture.bridge.handle_incoming(&second)
        );
    })
    .await
    .unwrap();
    assert_eq!(
        fixture
            .bindings
            .get_by_conversation(&fixture.owner, "test_conversation")
            .await
            .unwrap()
            .unwrap()
            .chat_id,
        "second_group"
    );
    assert_eq!(
        fixture
            .bindings
            .get_by_conversation(&fixture.owner, "second_conversation")
            .await
            .unwrap()
            .unwrap()
            .chat_id,
        "group"
    );
}

#[cfg(feature = "lark")]
#[tokio::test]
async fn raw_ingress_retry_cannot_follow_a_later_binding_into_another_conversation() {
    use crate::plugin::{DurableChannelIngress, DurableIngressEvent};
    let fixture = fixture().await;
    let original = bind(&fixture).await;
    let mut second = fixture
        .bridge
        .deps
        .conversation_repo
        .get(&fixture.owner, "test_conversation")
        .await
        .unwrap()
        .unwrap();
    second.id = "replacement_conversation".into();
    fixture.bridge.deps.conversation_repo.create(&second).await.unwrap();
    let code = fixture
        .bridge
        .create_binding_code(&fixture.owner, &second.id)
        .await
        .unwrap();
    let event = |id: &str, text: &str, union: &str| {
        DurableIngressEvent {
        id:format!("raw:{id}"),owner_user_id:fixture.owner.clone(),app_id:"app".into(),bot_open_id:"bot".into(),frame_type:"event".into(),
        payload:json!({"header":{"event_id":id,"event_type":"im.message.receive_v1","app_id":"app"},"event":{"sender":{"sender_id":{"open_id":"ou_member","union_id":union},"sender_type":"user","tenant_key":"tenant"},"message":{"message_id":id,"chat_id":"group","chat_type":"group","message_type":"text","content":json!({"text":text}).to_string(),"mentions":[{"key":"@bot","id":{"open_id":"bot"},"name":"Bot"}]}}}).to_string(),
    }
    };
    let old = event("old_raw", "belongs exclusively to original conversation", "on_other");
    fixture.bridge.accept(old.clone()).await.unwrap();
    fixture
        .bridge
        .accept(event("switch_raw", &format!("/bind {}", code.bind_code), "on_owner"))
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER reject_old_raw BEFORE INSERT ON feishu_inbox WHEN NEW.id='feishu:app:old_raw' BEGIN SELECT RAISE(FAIL,'temporary inbox failure'); END").execute(fixture._database.pool()).await.unwrap();
    fixture.bridge.flush_ingress().await.unwrap();
    assert_eq!(fixture.bindings.pending_ingress().await.unwrap().len(), 1);
    assert!(
        fixture
            .bindings
            .get_by_conversation(&fixture.owner, &original.conversation_id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        fixture
            .bindings
            .get_by_chat("app", "tenant", "group")
            .await
            .unwrap()
            .unwrap()
            .conversation_id,
        second.id
    );
    sqlx::query("DROP TRIGGER reject_old_raw")
        .execute(fixture._database.pool())
        .await
        .unwrap();
    fixture.bridge.flush_ingress().await.unwrap();
    assert!(
        fixture.bindings.pending_inbox().await.unwrap().is_empty(),
        "old accepted input must not be reinterpreted under the replacement binding"
    );
    assert!(fixture.bindings.pending_ingress().await.unwrap().is_empty());
    // Transport redelivery also cannot replace the first receipt's generation.
    fixture.bridge.accept(old).await.unwrap();
    fixture.bridge.flush_deliveries().await.unwrap();
    let copies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE id='feishu:app:old_raw'")
        .fetch_one(fixture._database.pool())
        .await
        .unwrap();
    assert_eq!(copies, 0);
}

#[tokio::test]
async fn unbind_waits_for_paused_question_card_send_before_returning() {
    let fixture = fixture().await;
    let binding = bind(&fixture).await;
    fixture.bridge.publish_question(&binding,"sensitive_question",&json!({"request_id":"paused_card","questions":[{"question":"Original conversation details","options":[]}]})).await.unwrap();
    fixture.sender.block_next.store(true, Ordering::SeqCst);
    let sender = tokio::spawn({
        let bridge = fixture.bridge.clone();
        async move { bridge.flush_interactions().await }
    });
    tokio::time::timeout(Duration::from_secs(2), fixture.sender.entered.notified())
        .await
        .unwrap();
    let owner = fixture.owner.clone();
    let mut unbinding = tokio::spawn({
        let bridge = fixture.bridge.clone();
        async move { bridge.unbind(&owner, "test_conversation").await }
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut unbinding)
            .await
            .is_err(),
        "owner unbind must wait for the old generation's in-flight card"
    );
    assert!(
        fixture
            .bindings
            .get_by_conversation(&fixture.owner, "test_conversation")
            .await
            .unwrap()
            .is_some()
    );
    fixture.sender.release.notify_one();
    sender.await.unwrap();
    unbinding.await.unwrap().unwrap();
    let sent_before = fixture.sender.sent.lock().unwrap().len();
    fixture.bridge.flush_interactions().await;
    assert_eq!(fixture.sender.sent.lock().unwrap().len(), sent_before);
}
