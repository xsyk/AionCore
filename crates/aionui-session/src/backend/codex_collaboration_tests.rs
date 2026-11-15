//! Collaboration contract regressions; protocol evidence lives in
//! docs/superpowers/sdd/1.0.3/protocol-captures/codex-0161-startup-schema-naehmui2/.
use super::*;
use crate::testing::FakeAgentIo;

fn catalog() -> Value {
    // Captured directory: r2-codex-plan-question-214.json (0.161.0).
    json!({"data":[
        {"name":"Plan", "mode":"plan", "model":null, "reasoning_effort":"medium"},
        {"name":"Default", "mode":"default", "model":null, "reasoning_effort":null}
    ]})
}

struct Harness {
    backend: Arc<CodexSessionBackend>,
    captured: Arc<Mutex<Vec<u8>>>,
    release: Box<dyn Fn() + Send + Sync>,
    events: BoxStream<'static, SessionEnvelope>,
}

fn settings(model: &str, effort: Value) -> Value {
    // Full legal notification shape, checked against the actual 0.161.0 schema
    // in protocol-captures/r3-codex-settings-214-kga2xz40/v2/.
    json!({"method":"thread/settings/updated", "params":{"threadId":"th1", "threadSettings":{
        "disabledPluginIds":[],"cwd":"/w","approvalPolicy":"untrusted","approvalsReviewer":"user",
        "sandboxPolicy":{"type":"readOnly","networkAccess":false},
        "activePermissionProfile":{"id":":workspace","extends":null},
        "model":model,"modelProvider":"openai","serviceTier":null,"effort":effort,"summary":null,
        "collaborationMode":{"mode":"default","settings":{"model":model,"reasoning_effort":effort,"developer_instructions":null}},
        "multiAgentMode":"explicitRequestOnly","personality":null
    }}})
}

impl Harness {
    async fn new(segments: Vec<Vec<Value>>) -> Self {
        Self::with_handshake(segments, HandshakeMode::Fresh).await
    }

    async fn with_handshake(mut segments: Vec<Vec<Value>>, mode: HandshakeMode<'_>) -> Self {
        segments.insert(
            0,
            vec![
                json!({"id":2,"result":{"thread":{"id":"th1"},"model":"actual-model","reasoningEffort":"low"}}),
                json!({"method":"thread/started","params":{"thread":{"id":"th1"}}}),
                json!({"id":5,"result":catalog()}),
            ],
        );
        let segments = segments
            .into_iter()
            .map(|mut frames| {
                frames.push(json!({"method":"mcpServer/startupStatus/updated","params":{"status":"ready"}}));
                frames
                    .into_iter()
                    .map(|v| format!("{v}\n"))
                    .collect::<String>()
                    .into_bytes()
            })
            .chain(std::iter::once(Vec::new()))
            .collect();
        let fake = FakeAgentIo::new(Vec::new(), None).with_gated_segments(segments);
        let captured = fake.captured_stdin();
        let release = Box::new(fake.segment_releaser());
        let backend = Arc::new(CodexSessionBackend::build_with_io("collaboration", Box::new(fake)).await);
        let events = backend.events();
        backend.run_handshake(mode).await.unwrap();
        let mut h = Self {
            backend,
            captured,
            release,
            events,
        };
        h.next().await;
        h
    }
    async fn next(&mut self) {
        (self.release)();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(env) = self.events.next().await {
                if matches!(
                    env.event,
                    SessionEvent::Provisioning {
                        phase: ProvisioningPhase::ToolsReady
                    }
                ) {
                    return;
                }
            }
            panic!("protocol segment ended before marker");
        })
        .await
        .unwrap();
    }
    async fn select(&self, mode: &str) -> Result<CommandReceipt, BackendError> {
        self.backend
            .dispatch(Command::SetConfigOption {
                option_id: "collaboration_mode".into(),
                value: mode.into(),
            })
            .await
    }
    async fn send(&self, text: &str) -> Result<CommandReceipt, BackendError> {
        self.backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text(text.into())],
                metadata: Default::default(),
            })
            .await
    }
    async fn send_after_readback(&self, text: &str) {
        let send = self.send(text);
        tokio::pin!(send);
        assert!(futures_util::poll!(&mut send).is_pending());
        assert_eq!(self.frames().await.last().unwrap()["method"], "thread/read");
        (self.release)();
        send.await.unwrap();
    }
    async fn frames(&self) -> Vec<Value> {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let frames: Vec<Value> = String::from_utf8(self.captured.lock().await.clone())
                    .unwrap()
                    .split_inclusive('\n')
                    .filter(|s| s.ends_with('\n'))
                    .map(|s| serde_json::from_str(s).unwrap())
                    .collect();
                if frames.last().and_then(|v| v.get("id")).and_then(Value::as_u64)
                    == Some(self.backend.rpc_id.load(Ordering::SeqCst))
                {
                    return frames;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }
}

#[test]
fn codex_collaboration_settings_notification_emits_typed_effort_including_null() {
    for effort in [json!("high"), Value::Null] {
        let frame = settings("actual-model", effort.clone());
        let events = map_notification("thread/settings/updated", &frame["params"]);
        assert!(
            events.iter().any(|event| matches!(event,
            SessionEvent::EffortConfirmed { effort:actual } if actual.as_deref() == effort.as_str())),
            "legal settings notification must propagate actual effort: {events:?}"
        );
    }
    let events = map_notification(
        "thread/settings/updated",
        &json!({"threadSettings":{"model":"actual-model"}}),
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, SessionEvent::EffortConfirmed { .. })),
        "an absent effort must not claim a confirmed null"
    );
}

// State-machine fixtures use the legal empty success and thread/read shape.
// Real CLI proof is r3-settings-readback-evidence.json; these are not live captures.
fn readback(id: u64, model: &str, effort: Value) -> Value {
    json!({"id":id,"result":{"thread":{"id":"th1","model":model,
        "reasoningEffort":effort,"status":{"type":"idle"},"turns":[]}}})
}

// These gated schedules prove the product state machine, not live CLI timing.
#[tokio::test(start_paused = true)]
async fn codex_collaboration_ack_wait_send_uses_actual_readback_after_all_receipts() {
    let mut h = Harness::new(vec![
        vec![json!({"id":6,"result":{}})],
        vec![json!({"id":7,"result":{}})],
        vec![readback(8, "readback-model", json!("medium"))],
    ])
    .await;
    h.select("plan").await.unwrap();
    h.backend
        .dispatch(Command::SetModel {
            model: "requested-model".into(),
        })
        .await
        .unwrap();
    h.backend
        .dispatch(Command::SetConfigOption {
            option_id: "effort".into(),
            value: "high".into(),
        })
        .await
        .unwrap();
    let backend = h.backend.clone();
    let send = backend.dispatch(Command::Send {
        content: vec![ContentBlock::Text("immediately after settings write".into())],
        metadata: Default::default(),
    });
    tokio::pin!(send);
    assert!(
        futures_util::poll!(&mut send).is_pending(),
        "Send must wait for settings ACKs instead of immediately rejecting"
    );
    let frames = h.frames().await;
    assert_eq!(frames.last().unwrap()["method"], "thread/settings/update");
    assert!(
        !frames
            .iter()
            .any(|f| f["method"] == "turn/start" || f["method"] == "thread/read")
    );
    h.next().await;
    tokio::time::advance(std::time::Duration::from_millis(10)).await;
    assert!(futures_util::poll!(&mut send).is_pending());
    assert!(
        !h.frames()
            .await
            .iter()
            .any(|f| f["method"] == "turn/start" || f["method"] == "thread/read"),
        "one receipt cannot cover the other unacknowledged settings write"
    );
    h.next().await;
    tokio::time::advance(std::time::Duration::from_millis(10)).await;
    assert!(futures_util::poll!(&mut send).is_pending());
    let frames = h.frames().await;
    assert_eq!(frames.last().unwrap()["method"], "thread/read");
    assert!(!frames.iter().any(|f| f["method"] == "turn/start"));
    h.next().await;
    send.await.unwrap();
    let frames = h.frames().await;
    let turns: Vec<_> = frames.iter().filter(|f| f["method"] == "turn/start").collect();
    assert_eq!(turns.len(), 1);
    assert_eq!(
        turns[0]["params"]["collaborationMode"],
        json!({"mode":"plan","settings":{"model":"readback-model","reasoning_effort":"medium","developer_instructions":null}})
    );
}

#[tokio::test(start_paused = true)]
async fn codex_collaboration_ack_wait_timeout_keeps_send_off_wire() {
    let h = Harness::new(vec![]).await;
    h.select("plan").await.unwrap();
    h.backend
        .dispatch(Command::SetConfigOption {
            option_id: "effort".into(),
            value: "high".into(),
        })
        .await
        .unwrap();
    let send = h.send("no receipt");
    tokio::pin!(send);
    assert!(
        futures_util::poll!(&mut send).is_pending(),
        "missing ACK must receive a bounded wait before rejection"
    );
    let budget = crate::backend::handshake_budget();
    tokio::time::advance(budget - std::time::Duration::from_millis(10)).await;
    assert!(futures_util::poll!(&mut send).is_pending());
    tokio::time::advance(std::time::Duration::from_millis(10)).await;
    let err = send.await.expect_err("missing ACK must time out");
    assert!(
        err.to_string().contains("settings pending confirmation (ack timeout)"),
        "{err}"
    );
    assert!(h.backend.collaboration.lock().unwrap().pending());
    assert!(!h.backend.turn_in_flight.load(Ordering::SeqCst));
    assert_eq!(h.backend.turn_gen.load(Ordering::SeqCst), 0);
    assert!(
        !h.frames()
            .await
            .iter()
            .any(|f| f["method"] == "turn/start" || f["method"] == "thread/read")
    );
}

#[tokio::test(start_paused = true)]
async fn codex_collaboration_ack_wait_notification_cannot_send_before_receipt() {
    let mut h = Harness::new(vec![
        vec![settings("notified-model", json!("high"))],
        vec![json!({"id":6,"result":{}})],
    ])
    .await;
    h.select("plan").await.unwrap();
    h.backend
        .dispatch(Command::SetConfigOption {
            option_id: "effort".into(),
            value: "high".into(),
        })
        .await
        .unwrap();
    let backend = h.backend.clone();
    let send = backend.dispatch(Command::Send {
        content: vec![ContentBlock::Text("notification before ACK".into())],
        metadata: Default::default(),
    });
    tokio::pin!(send);
    assert!(futures_util::poll!(&mut send).is_pending(), "Send must wait for ACK");
    h.next().await;
    tokio::time::advance(std::time::Duration::from_millis(10)).await;
    assert!(
        futures_util::poll!(&mut send).is_pending(),
        "a matching notification alone must not release Send"
    );
    assert!(h.backend.collaboration.lock().unwrap().pending());
    assert!(
        !h.frames()
            .await
            .iter()
            .any(|f| f["method"] == "turn/start" || f["method"] == "thread/read")
    );
    h.next().await;
    send.await.unwrap();
    let frames = h.frames().await;
    let turns: Vec<_> = frames.iter().filter(|f| f["method"] == "turn/start").collect();
    assert_eq!(turns.len(), 1);
    assert_eq!(
        turns[0]["params"]["collaborationMode"],
        json!({"mode":"plan","settings":{"model":"notified-model","reasoning_effort":"high","developer_instructions":null}})
    );
    assert!(!frames.iter().any(|f| f["method"] == "thread/read"));
}

async fn assert_cancel_prevents_waiting_send(target: CancelTarget, block_stdin: bool) {
    let mut h = Harness::new(vec![vec![
        json!({"id":6,"result":{}}),
        settings("confirmed-after-cancel", json!("high")),
    ]])
    .await;
    h.select("plan").await.unwrap();
    h.backend
        .dispatch(Command::SetConfigOption {
            option_id: "effort".into(),
            value: "high".into(),
        })
        .await
        .unwrap();
    if block_stdin {
        h.next().await;
    }
    let backend = h.backend.clone();
    let stdin = if block_stdin {
        Some(backend.stdin.lock().await)
    } else {
        None
    };
    let send = backend.dispatch(Command::Send {
        content: vec![ContentBlock::Text("cancel this waiting Send".into())],
        metadata: Default::default(),
    });
    tokio::pin!(send);
    assert!(futures_util::poll!(&mut send).is_pending());
    let receipt = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        h.backend.dispatch(Command::Cancel { target }),
    )
    .await
    .expect("Cancel must not wait for settings ACK or the blocked stdin")
    .unwrap();
    assert!(receipt.accepted);
    assert_eq!(receipt.admission, Admission::NoTurn);
    assert_eq!(receipt.turn_gen, 0);
    if !block_stdin {
        assert!(
            h.backend.collaboration.lock().unwrap().pending(),
            "Cancel must preserve settings pending"
        );
        h.next().await;
    }
    drop(stdin);
    let err = send
        .await
        .expect_err("a successfully cancelled pre-wire Send must not start later");
    assert!(err.to_string().contains("send cancelled before turn start"), "{err}");
    assert_eq!(h.backend.turn_gen.load(Ordering::SeqCst), 0);
    assert!(!h.backend.turn_in_flight.load(Ordering::SeqCst));
    assert!(h.backend.pending_sends.lock().await.is_empty());
    // Read captured bytes directly: a cancelled RPC reservation need not be written.
    let frames: Vec<Value> = String::from_utf8(h.captured.lock().await.clone())
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert!(
        !frames
            .iter()
            .any(|f| f["method"] == "turn/start" || f["method"] == "turn/interrupt")
    );
    let receipt = h.send("new Send after cancellation").await.unwrap();
    assert_eq!(receipt.admission, Admission::Started);
    assert_eq!(receipt.turn_gen, 1);
    let frames = h.frames().await;
    let turns: Vec<_> = frames.iter().filter(|f| f["method"] == "turn/start").collect();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0]["params"]["input"][0]["text"], "new Send after cancellation");
    assert_eq!(
        turns[0]["params"]["collaborationMode"]["settings"],
        json!({"model":"confirmed-after-cancel","reasoning_effort":"high","developer_instructions":null})
    );
}

#[tokio::test(start_paused = true)]
async fn codex_collaboration_ack_wait_cancel_turn_before_ack_prevents_start() {
    assert_cancel_prevents_waiting_send(CancelTarget::Turn, false).await;
}

#[tokio::test(start_paused = true)]
async fn codex_collaboration_ack_wait_cancel_session_before_ack_prevents_start() {
    assert_cancel_prevents_waiting_send(CancelTarget::Session, false).await;
}

#[tokio::test(start_paused = true)]
async fn codex_collaboration_ack_wait_cancel_turn_before_write_prevents_start() {
    assert_cancel_prevents_waiting_send(CancelTarget::Turn, true).await;
}

#[tokio::test(start_paused = true)]
async fn codex_collaboration_ack_wait_cancel_session_before_write_prevents_start() {
    assert_cancel_prevents_waiting_send(CancelTarget::Session, true).await;
}

async fn assert_cancelled_readback_cannot_replace_new_actual(target: CancelTarget) {
    // Product-only schedule: nonempty actual receipts are an existing supported
    // branch, not a claim about the real CLI's response shape or timing.
    let mut h = Harness::new(vec![
        vec![json!({"id":6,"result":{}})],
        vec![json!({"id":8,"result":{"threadSettings":{"model":"new-actual","effort":"medium"}}})],
        vec![readback(7, "old-actual", json!("high"))],
        vec![readback(9, "new-actual", json!("medium"))],
    ])
    .await;
    h.select("plan").await.unwrap();
    h.backend
        .dispatch(Command::SetConfigOption {
            option_id: "effort".into(),
            value: "high".into(),
        })
        .await
        .unwrap();
    h.next().await;
    let backend = h.backend.clone();
    let send = backend.dispatch(Command::Send {
        content: vec![ContentBlock::Text("cancel while readback is pending".into())],
        metadata: Default::default(),
    });
    tokio::pin!(send);
    assert!(futures_util::poll!(&mut send).is_pending());
    let frames = h.frames().await;
    assert_eq!(frames.last().unwrap()["method"], "thread/read");
    assert_eq!(frames.last().unwrap()["id"], 7);
    let receipt = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        backend.dispatch(Command::Cancel { target }),
    )
    .await
    .expect("Cancel must not wait for the readback response")
    .unwrap();
    assert!(receipt.accepted);
    assert_eq!(receipt.admission, Admission::NoTurn);
    assert_eq!(receipt.turn_gen, 0);
    let err = send.await.expect_err("cancelled Send must not start");
    assert!(err.to_string().contains("send cancelled before turn start"), "{err}");
    assert_eq!(backend.turn_gen.load(Ordering::SeqCst), 0);
    assert!(!backend.turn_in_flight.load(Ordering::SeqCst));
    assert!(backend.pending_sends.lock().await.is_empty());
    assert!(
        backend.collaboration.lock().unwrap().pending(),
        "Cancel must retain the original acknowledged settings"
    );
    backend
        .dispatch(Command::SetModel {
            model: "new-requested".into(),
        })
        .await
        .unwrap();
    h.next().await;
    {
        let state = backend.collaboration.lock().unwrap();
        assert_eq!(state.model.as_deref(), Some("new-actual"));
        assert_eq!(state.effort.as_deref(), Some("medium"));
        assert!(state.pending(), "the older setting still needs actual confirmation");
    }
    h.next().await;
    {
        let state = backend.collaboration.lock().unwrap();
        assert_eq!(
            state.model.as_deref(),
            Some("new-actual"),
            "cancelled readback must not overwrite a newer actual model"
        );
        assert_eq!(
            state.effort.as_deref(),
            Some("medium"),
            "cancelled readback must not overwrite newer actual effort"
        );
        assert!(
            state.pending(),
            "late cancelled readback must not retire settings pending"
        );
    }
    assert!(
        !h.frames()
            .await
            .iter()
            .any(|f| f["method"] == "turn/start" || f["method"] == "turn/interrupt")
    );
    let next = backend.dispatch(Command::Send {
        content: vec![ContentBlock::Text("new Send requires current confirmation".into())],
        metadata: Default::default(),
    });
    tokio::pin!(next);
    assert!(futures_util::poll!(&mut next).is_pending());
    let frames = h.frames().await;
    assert_eq!(frames.last().unwrap()["method"], "thread/read");
    assert_eq!(frames.last().unwrap()["id"], 9);
    assert!(!frames.iter().any(|f| f["method"] == "turn/start"));
    h.next().await;
    let receipt = next.await.unwrap();
    assert_eq!(receipt.admission, Admission::Started);
    assert_eq!(receipt.turn_gen, 1);
    let frames = h.frames().await;
    let turns: Vec<_> = frames.iter().filter(|f| f["method"] == "turn/start").collect();
    assert_eq!(turns.len(), 1);
    assert_eq!(
        turns[0]["params"]["collaborationMode"]["settings"],
        json!({"model":"new-actual","reasoning_effort":"medium","developer_instructions":null})
    );
    assert!(!backend.collaboration.lock().unwrap().pending());
}

#[tokio::test(start_paused = true)]
async fn codex_collaboration_ack_wait_cancel_readback_turn_preserves_new_actual() {
    assert_cancelled_readback_cannot_replace_new_actual(CancelTarget::Turn).await;
}

#[tokio::test(start_paused = true)]
async fn codex_collaboration_ack_wait_cancel_readback_session_preserves_new_actual() {
    assert_cancelled_readback_cannot_replace_new_actual(CancelTarget::Session).await;
}

#[tokio::test]
async fn codex_collaboration_empty_ack_without_notification_converges_via_readback() {
    for requested in ["actual-model", "changed-model"] {
        let mut h = Harness::new(vec![
            vec![json!({"id":6,"result":{}})],
            vec![readback(7, requested, json!("high"))],
        ])
        .await;
        h.select("plan").await.unwrap();
        h.backend
            .dispatch(Command::SetModel {
                model: requested.into(),
            })
            .await
            .unwrap();
        h.next().await;
        {
            let send = h.send("after empty success");
            tokio::pin!(send);
            assert!(
                futures_util::poll!(&mut send).is_pending(),
                "legal empty success must start an actual readback instead of rejecting forever"
            );
            let frames = h.frames().await;
            let frame = frames.last().unwrap();
            assert_eq!(frame["method"], "thread/read");
            assert_eq!(frame["params"], json!({"threadId":"th1","includeTurns":false}));
            (h.release)();
            send.await.unwrap();
        }
        let frame = h.frames().await.pop().unwrap();
        assert_eq!(frame["method"], "turn/start");
        assert_eq!(frame["params"]["collaborationMode"]["settings"]["model"], requested);
        assert_eq!(
            frame["params"]["collaborationMode"]["settings"]["reasoning_effort"],
            "high"
        );
    }
}

#[tokio::test]
async fn codex_collaboration_other_thread_notification_cannot_confirm_this_request() {
    let mut other = settings("requested-model", json!("high"));
    other["params"]["threadId"] = json!("another-thread");
    let mut h = Harness::new(vec![vec![json!({"id":6,"result":{}}), other]]).await;
    h.select("plan").await.unwrap();
    h.backend
        .dispatch(Command::SetModel {
            model: "requested-model".into(),
        })
        .await
        .unwrap();
    h.next().await;
    let state = h.backend.collaboration.lock().unwrap();
    assert!(
        state.pending(),
        "another thread cannot confirm this request even if values match"
    );
    assert_eq!(state.model.as_deref(), Some("actual-model"));
    assert_eq!(state.effort.as_deref(), Some("low"));
}

#[tokio::test]
async fn codex_collaboration_unrelated_notification_does_not_clear_pending_model() {
    let mut h = Harness::new(vec![vec![
        json!({"id":6,"result":{}}),
        settings("actual-model", json!("high")),
    ]])
    .await;
    h.select("plan").await.unwrap();
    h.backend
        .dispatch(Command::SetModel {
            model: "not-yet-confirmed".into(),
        })
        .await
        .unwrap();
    h.next().await;
    assert!(
        h.backend.collaboration.lock().unwrap().pending(),
        "a notification for another setting cannot confirm this model request"
    );
}

#[tokio::test]
async fn codex_collaboration_adjacent_empty_acks_read_final_state_before_default() {
    let mut h = Harness::new(vec![
        vec![json!({"id":6,"result":{}}), json!({"id":7,"result":{}})],
        vec![readback(8, "actual-model", json!("high"))],
    ])
    .await;
    h.select("plan").await.unwrap();
    for effort in ["low", "high"] {
        h.backend
            .dispatch(Command::SetConfigOption {
                option_id: "effort".into(),
                value: effort.into(),
            })
            .await
            .unwrap();
    }
    h.next().await;
    {
        let switch = h.select("default");
        tokio::pin!(switch);
        assert!(futures_util::poll!(&mut switch).is_pending());
        assert_eq!(h.frames().await.last().unwrap()["method"], "thread/read");
        (h.release)();
        switch.await.unwrap();
    }
    h.send("after merged updates").await.unwrap();
    assert_eq!(
        h.frames().await.last().unwrap()["params"]["collaborationMode"],
        json!({"mode":"default","settings":{"model":"actual-model","reasoning_effort":"high","developer_instructions":null}})
    );
}

#[test]
fn codex_collaboration_pending_is_independent_of_confirmed_notification() {
    let mut state = Collaboration::default();
    state.notification(&json!({"model":"old","effort":"low"}));
    state.register_settings(10, json!({"effort":"high"}));
    state.register_settings(11, json!({"model":"new"}));
    state.settings_response(10, &json!({"result":{}}));
    state.settings_response(11, &json!({"result":{}}));
    state.notification(&json!({"model":"old","effort":"high"}));
    assert_eq!(state.effort.as_deref(), Some("high"));
    assert!(
        state.pending(),
        "effort confirmation must not retire the new-model request"
    );
    assert!(state.begin_readback(12, "th1"));
    assert!(state.readback_response(12, &readback(12, "new", json!("high"))));
    assert!(!state.pending());
    assert_eq!(state.model.as_deref(), Some("new"));
}

#[test]
fn codex_collaboration_readback_does_not_retire_a_later_unacknowledged_request() {
    let mut state = Collaboration::default();
    state.selected = Some("plan".into());
    state.notification(&json!({"model":"old","effort":"low"}));
    state.register_settings(10, json!({"effort":"high"}));
    state.settings_response(10, &json!({"result":{}}));
    assert!(state.begin_readback(11, "th1"));
    state.register_settings(12, json!({"model":"new"}));
    assert!(state.readback_response(11, &readback(11, "old", json!("high"))));
    assert!(state.pending());
    assert!(state.turn_value().unwrap_err().to_string().contains("pending"));
    state.notification(&json!({"model":"old","effort":"high"}));
    state.settings_response(12, &json!({"result":{}}));
    assert!(
        state.pending(),
        "even an acknowledgement after an unrelated notification needs observation"
    );
    state.forget_readback(11);
    assert!(state.begin_readback(13, "th1"));
    state.reset_process();
    assert!(!state.readback_response(13, &readback(13, "old-process", json!("high"))));
    assert_eq!(state.model, None);
}

#[test]
fn codex_collaboration_readback_rejects_missing_unloaded_wrong_thread_and_stale_data() {
    for bad in [
        json!({"id":13,"error":{"code":-32600}}),
        json!({"id":13,"result":{}}),
        json!({"id":13,"result":{"thread":{"id":"th1","model":"new","status":{"type":"idle"}}}}),
        json!({"id":13,"result":{"thread":{"id":"other","model":"new","reasoningEffort":"high","status":{"type":"idle"}}}}),
        json!({"id":13,"result":{"thread":{"id":"th1","model":"new","reasoningEffort":"high","status":{"type":"notLoaded"}}}}),
    ] {
        let mut state = Collaboration::default();
        state.notification(&json!({"model":"old","effort":"low"}));
        state.register_settings(10, json!({"model":"requested"}));
        state.settings_response(10, &json!({"result":{}}));
        assert!(state.begin_readback(13, "th1"));
        assert!(!state.readback_response(13, &bad));
        assert!(state.pending());
        assert_eq!(state.model.as_deref(), Some("old"));
        state.forget_readback(13);
        assert!(!state.readback_response(13, &readback(13, "late", Value::Null)));
        assert!(state.begin_readback(14, "th1"));
        state.notification(&json!({"model":"newer","effort":"high"}));
        assert!(!state.readback_response(14, &readback(14, "stale", json!("low"))));
        assert_eq!(state.model.as_deref(), Some("newer"));
        assert!(state.pending());
        state.forget_readback(14);
        assert!(state.begin_readback(15, "th1"));
        assert!(state.readback_response(15, &readback(15, "actual", Value::Null)));
        assert_eq!(state.effort, None);
        assert!(!state.pending());
    }
}

fn completed() -> Value {
    json!({"method":"turn/completed","params":{"threadId":"th1","turn":{"id":"turn1","status":"completed"}}})
}

#[tokio::test]
async fn codex_collaboration_turn_uses_actual_model_and_effort_for_plan_and_default() {
    let mut h = Harness::new(vec![vec![completed(), settings("new-model", json!("high"))]]).await;
    h.select("plan").await.unwrap();
    h.send("hello").await.unwrap();
    let first = h.frames().await.pop().unwrap();
    assert_eq!(
        first["params"]["collaborationMode"],
        json!({"mode":"plan","settings":{"model":"actual-model","reasoning_effort":"low","developer_instructions":null}})
    );
    h.next().await;
    h.select("default").await.unwrap();
    h.send("/init").await.unwrap();
    let second = h.frames().await.pop().unwrap();
    assert_eq!(
        second["params"]["collaborationMode"],
        json!({"mode":"default","settings":{"model":"new-model","reasoning_effort":"high","developer_instructions":null}})
    );
}

#[tokio::test]
async fn codex_collaboration_busy_rejects_before_turn_started_notification() {
    let h = Harness::new(vec![]).await;
    h.select("plan").await.unwrap();
    h.send("hello").await.unwrap();
    let err = h.select("default").await.expect_err("busy mode switch must reject");
    assert!(err.to_string().contains("busy"), "{err}");
}

#[tokio::test(start_paused = true)]
async fn codex_collaboration_pending_and_failed_settings_do_not_replace_actual_model() {
    let mut h = Harness::new(vec![vec![json!({"id":6,"error":{"code":-32600,"message":"rejected"}})]]).await;
    h.select("plan").await.unwrap();
    h.backend
        .dispatch(Command::SetModel {
            model: "unconfirmed-model".into(),
        })
        .await
        .unwrap();
    let err = h
        .send("hello")
        .await
        .expect_err("missing settings receipt must time out");
    assert!(
        err.to_string().contains("settings pending confirmation (ack timeout)"),
        "{err}"
    );
    let err = h
        .select("default")
        .await
        .expect_err("mode switch must also time out without a settings receipt");
    assert!(
        err.to_string().contains("settings pending confirmation (ack timeout)"),
        "{err}"
    );
    assert!(
        !h.frames()
            .await
            .iter()
            .any(|f| f["method"] == "turn/start" || f["method"] == "thread/read")
    );
    assert_eq!(
        h.backend.collaboration.lock().unwrap().model.as_deref(),
        Some("actual-model")
    );
    assert_eq!(
        h.backend.collaboration.lock().unwrap().selected.as_deref(),
        Some("plan")
    );
    h.next().await;
    h.send("hello").await.unwrap();
    let frame = h.frames().await.pop().unwrap();
    assert_eq!(
        frame["params"]["collaborationMode"]["settings"]["model"],
        "actual-model"
    );
}

#[tokio::test]
async fn codex_collaboration_fresh_rejection_fails_fast_with_safe_code() {
    let fake = FakeAgentIo::new(Vec::new(), None).with_gated_tail(
        format!(
            "{}\n",
            json!({"id":2,"error":{"code":-32600,"message":"private provider payload"}})
        )
        .into_bytes(),
    );
    let release = fake.stdout_releaser();
    let backend = CodexSessionBackend::build_with_io("collaboration", Box::new(fake)).await;
    backend.run_handshake(HandshakeMode::Fresh).await.unwrap();
    release();
    let result = backend.bound_thread_within(std::time::Duration::from_millis(200)).await;
    let err = result.expect_err("startup rejected");
    assert!(
        err.to_string().contains("-32600"),
        "must retain safe error code, got {err}"
    );
    assert!(!err.to_string().contains("private provider payload"));
    assert!(
        matches!(err, BackendError::Transport(_)),
        "fresh rejection must not trigger dead-session recovery"
    );
}

#[tokio::test]
async fn codex_collaboration_resume_waits_for_actual_receipt() {
    let h = Harness::new(vec![]).await;
    h.select("plan").await.unwrap();
    h.backend.collaboration.lock().unwrap().reset_process();
    let fake = FakeAgentIo::new(Vec::new(), None).with_gated_segments(vec![
        format!(
            "{}\n",
            json!({"id":7,"result":{"thread":{"id":"th1"},"model":"resumed-model","reasoningEffort":null}})
        )
        .into_bytes(),
        Vec::new(),
    ]);
    let release = fake.segment_releaser();
    let io: Arc<dyn AgentIo> = Arc::new(fake);
    let (stdin, stdout) = io.take_stdio().await.unwrap();
    *h.backend.stdin.lock().await = Some(stdin);
    let reader = start_codex_reader(&h.backend.reader_state, Some(stdout), io);
    h.backend.run_handshake(HandshakeMode::Resume("th1")).await.unwrap();
    let release_task = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        release();
    });
    h.backend
        .dispatch(Command::Send {
            content: vec![ContentBlock::Text("after wake".into())],
            metadata: Default::default(),
        })
        .await
        .expect("resume must wait for actual model instead of using stale values or failing immediately");
    release_task.await.unwrap();
    let state = h.backend.collaboration.lock().unwrap();
    assert_eq!(state.selected.as_deref(), Some("plan"));
    assert_eq!(state.model.as_deref(), Some("resumed-model"));
    assert_eq!(state.effort, None);
    drop(state);
    reader.abort();
}

#[tokio::test(start_paused = true)]
async fn codex_collaboration_empty_settings_ack_waits_for_notification() {
    let mut h = Harness::new(vec![
        vec![json!({"id":6,"result":{}})],
        vec![settings("actual-model", json!("xhigh"))],
    ])
    .await;
    h.select("plan").await.unwrap();
    h.backend
        .dispatch(Command::SetConfigOption {
            option_id: "effort".into(),
            value: "xhigh".into(),
        })
        .await
        .unwrap();
    h.next().await;
    let err = h
        .send("pending")
        .await
        .expect_err("empty ACK alone cannot confirm settings");
    assert!(
        err.to_string()
            .contains("settings pending confirmation (readback timeout)"),
        "{err}"
    );
    assert!(!h.frames().await.iter().any(|f| f["method"] == "turn/start"));
    h.next().await;
    h.send("confirmed").await.unwrap();
    assert_eq!(
        h.frames().await.pop().unwrap()["params"]["collaborationMode"]["settings"]["reasoning_effort"],
        "xhigh"
    );
}

#[tokio::test]
async fn codex_collaboration_late_settings_reply_does_not_undo_newer_notification() {
    let mut h = Harness::new(vec![
        vec![
            settings("latest-model", json!("high")),
            json!({"id":6,"result":{"threadSettings":{"model":"stale-model","effort":"low"}}}),
        ],
        vec![readback(7, "latest-model", json!("high"))],
    ])
    .await;
    h.select("plan").await.unwrap();
    h.backend
        .dispatch(Command::SetModel {
            model: "requested-model".into(),
        })
        .await
        .unwrap();
    h.next().await;
    // The unrelated notification cannot retire this request; read actual state.
    h.send_after_readback("confirmed").await;
    let actual = h.frames().await.pop().unwrap()["params"]["collaborationMode"]["settings"].clone();
    assert_eq!(actual["model"], "latest-model");
    assert_eq!(actual["reasoning_effort"], "high");
}

#[tokio::test]
async fn codex_collaboration_actual_settings_reply_confirms_model() {
    let mut h = Harness::new(vec![vec![
        json!({"id":6,"result":{"threadSettings":{"model":"confirmed-model","effort":null}}}),
    ]])
    .await;
    h.select("plan").await.unwrap();
    h.backend
        .dispatch(Command::SetModel {
            model: "requested-model".into(),
        })
        .await
        .unwrap();
    h.next().await;
    h.send("confirmed").await.unwrap();
    let actual = h.frames().await.pop().unwrap()["params"]["collaborationMode"]["settings"].clone();
    assert_eq!(actual["model"], "confirmed-model");
    assert_eq!(actual["reasoning_effort"], Value::Null);
}

#[tokio::test]
async fn codex_collaboration_implicit_and_special_turns_preserve_cli_semantics() {
    let mut h = Harness::new(vec![vec![completed()], vec![completed()]]).await;
    h.send("ordinary").await.unwrap();
    assert!(
        h.frames().await.pop().unwrap()["params"]
            .get("collaborationMode")
            .is_none()
    );
    h.next().await;
    h.select("plan").await.unwrap();
    h.send("/review").await.unwrap();
    let review = h.frames().await.pop().unwrap();
    assert_eq!(review["method"], "review/start");
    assert!(review["params"].get("collaborationMode").is_none());
    h.next().await;
    h.send("/compact").await.unwrap();
    let compact = h.frames().await.pop().unwrap();
    assert_eq!(compact["method"], "thread/compact/start");
    assert!(compact["params"].get("collaborationMode").is_none());
}

#[tokio::test]
async fn codex_collaboration_invalid_selection_preserves_token() {
    let h = Harness::new(vec![]).await;
    h.select("plan").await.unwrap();
    for invalid in ["auto", "Plan", "unknown", ""] {
        assert!(h.select(invalid).await.unwrap_err().to_string().contains("unavailable"));
        assert_eq!(
            h.backend.capabilities().current_collaboration_mode.as_deref(),
            Some("plan")
        );
    }
}

#[tokio::test]
async fn codex_collaboration_send_and_mode_switch_share_one_boundary() {
    let h = Harness::new(vec![]).await;
    h.select("plan").await.unwrap();
    let backend = Arc::new(h.backend);
    let stdin = backend.stdin.lock().await;
    let send_backend = backend.clone();
    let send = tokio::spawn(async move {
        send_backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("race".into())],
                metadata: Default::default(),
            })
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !backend.turn_in_flight.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let switch_backend = backend.clone();
    let switch = tokio::spawn(async move {
        switch_backend
            .dispatch(Command::SetConfigOption {
                option_id: "collaboration_mode".into(),
                value: "default".into(),
            })
            .await
    });
    tokio::task::yield_now().await;
    assert!(
        !switch.is_finished(),
        "switch must serialize behind the blocked new-turn write"
    );
    drop(stdin);
    send.await.unwrap().unwrap();
    assert!(switch.await.unwrap().unwrap_err().to_string().contains("busy"));
    assert_eq!(
        backend.capabilities().current_collaboration_mode.as_deref(),
        Some("plan")
    );
}

#[tokio::test]
async fn codex_collaboration_unselected_and_special_turns_preserve_cli_defaults() {
    for text in ["hello", "/init", "/review", "/compact"] {
        let h = Harness::new(vec![]).await;
        if text.starts_with("/review") || text == "/compact" {
            h.select("plan").await.unwrap();
        }
        h.send(text).await.unwrap();
        let frame = h.frames().await.pop().unwrap();
        assert!(frame["params"].get("collaborationMode").is_none(), "{frame}");
    }
}

#[tokio::test(start_paused = true)]
async fn codex_collaboration_empty_settings_ack_waits_for_actual_confirmation() {
    let mut h = Harness::new(vec![
        vec![json!({"id":6,"result":{}})],
        vec![settings("confirmed-model", Value::Null)],
    ])
    .await;
    h.select("plan").await.unwrap();
    h.backend
        .dispatch(Command::SetModel {
            model: "confirmed-model".into(),
        })
        .await
        .unwrap();
    h.next().await;
    let err = h
        .send("hello")
        .await
        .expect_err("empty ack cannot confirm a requested value");
    assert!(
        err.to_string()
            .contains("settings pending confirmation (readback timeout)"),
        "{err}"
    );
    assert!(!h.frames().await.iter().any(|f| f["method"] == "turn/start"));
    h.next().await;
    h.send("hello").await.unwrap();
    let frame = h.frames().await.pop().unwrap();
    assert_eq!(
        frame["params"]["collaborationMode"]["settings"],
        json!({"model":"confirmed-model","reasoning_effort":null,"developer_instructions":null})
    );
}

#[tokio::test]
async fn codex_collaboration_late_settings_reply_cannot_overwrite_newer_notification() {
    let mut h = Harness::new(vec![
        vec![
            settings("latest-model", json!("xhigh")),
            json!({"id":6,"result":{"threadSettings":{"model":"old-model","effort":"low"}}}),
        ],
        vec![readback(7, "latest-model", json!("xhigh"))],
    ])
    .await;
    h.select("plan").await.unwrap();
    h.backend
        .dispatch(Command::SetModel {
            model: "old-model".into(),
        })
        .await
        .unwrap();
    h.next().await;
    h.send_after_readback("hello").await;
    let frame = h.frames().await.pop().unwrap();
    assert_eq!(
        frame["params"]["collaborationMode"]["settings"]["model"],
        "latest-model"
    );
    assert_eq!(
        frame["params"]["collaborationMode"]["settings"]["reasoning_effort"],
        "xhigh"
    );
}

#[tokio::test(start_paused = true)]
async fn codex_collaboration_effort_confirmation_is_used_by_next_turn() {
    let mut h = Harness::new(vec![vec![
        json!({"id":6,"result":{"threadSettings":{"model":"actual-model","effort":"high"}}}),
    ]])
    .await;
    h.select("plan").await.unwrap();
    h.backend
        .dispatch(Command::SetConfigOption {
            option_id: "effort".into(),
            value: "high".into(),
        })
        .await
        .unwrap();
    let err = h.send("hello").await.expect_err("missing effort receipt must time out");
    assert!(
        err.to_string().contains("settings pending confirmation (ack timeout)"),
        "{err}"
    );
    assert!(
        !h.frames()
            .await
            .iter()
            .any(|f| f["method"] == "turn/start" || f["method"] == "thread/read")
    );
    h.next().await;
    h.send("hello").await.unwrap();
    assert_eq!(
        h.frames().await.pop().unwrap()["params"]["collaborationMode"]["settings"]["reasoning_effort"],
        "high"
    );
}

#[tokio::test]
async fn codex_collaboration_send_and_switch_share_one_serial_boundary() {
    let h = Harness::new(vec![]).await;
    h.select("plan").await.unwrap();
    let boundary = h.backend.dispatch_boundary.lock().await;
    let send = h.send("hello");
    let switch = h.select("default");
    tokio::pin!(send, switch);
    assert!(futures_util::poll!(&mut send).is_pending());
    assert!(futures_util::poll!(&mut switch).is_pending());
    drop(boundary);
    let (send, switch) = tokio::join!(send, switch);
    send.unwrap();
    assert!(switch.unwrap_err().to_string().contains("busy"));
    assert_eq!(
        h.backend.capabilities().current_collaboration_mode.as_deref(),
        Some("plan")
    );
}

#[tokio::test]
async fn codex_collaboration_replacement_reader_ignores_late_old_process_settings() {
    let mut h = Harness::new(vec![vec![settings("old-process-model", json!("low"))]]).await;
    h.select("plan").await.unwrap();
    h.backend.collaboration.lock().unwrap().reset_process();
    // A reader launched before reset must not mutate the replacement's state.
    (h.release)();
    tokio::task::yield_now().await;
    {
        let state = h.backend.collaboration.lock().unwrap();
        assert_eq!(state.selected.as_deref(), Some("plan"));
        assert_eq!(state.model, None);
    }
    // Re-attach through the same production reader and handshake as idle wake.
    let fake = FakeAgentIo::new(Vec::new(), None).with_gated_segments(vec![
        format!(
            "{}\n",
            json!({"id":7,"result":{"thread":{"id":"th1"},"model":"wake-model","reasoningEffort":"high"}})
        )
        .into_bytes(),
        Vec::new(),
    ]);
    let release = fake.segment_releaser();
    let captured = fake.captured_stdin();
    let io: Arc<dyn AgentIo> = Arc::new(fake);
    let (stdin, stdout) = io.take_stdio().await.unwrap();
    *h.backend.stdin.lock().await = Some(stdin);
    let reader = start_codex_reader(&h.backend.reader_state, Some(stdout), io);
    h.backend.run_handshake(HandshakeMode::Resume("th1")).await.unwrap();
    h.captured = captured;
    let send = h.send("hello");
    tokio::pin!(send);
    assert!(
        futures_util::poll!(&mut send).is_pending(),
        "wake Send must await the replacement thread receipt"
    );
    release();
    send.await.unwrap();
    assert_eq!(
        h.frames().await.pop().unwrap()["params"]["collaborationMode"]["settings"]["model"],
        "wake-model"
    );
    reader.abort();
}

#[tokio::test]
async fn codex_collaboration_fresh_resume_and_fork_receipts_confirm_actual_settings() {
    for mode in [
        HandshakeMode::Fresh,
        HandshakeMode::Resume("th1"),
        HandshakeMode::Fork {
            parent_thread_id: "parent",
            last_turn_id: Some("parent-turn"),
        },
    ] {
        let h = Harness::with_handshake(vec![], mode).await;
        h.select("plan").await.unwrap();
        h.send("after thread receipt").await.unwrap();
        let frame = h.frames().await.pop().unwrap();
        assert_eq!(frame["params"]["threadId"], "th1");
        assert_eq!(
            frame["params"]["collaborationMode"]["settings"],
            json!({
                "model":"actual-model", "reasoning_effort":"low", "developer_instructions":null
            })
        );
    }
}

#[tokio::test]
async fn codex_collaboration_consecutive_actual_settings_receipts_keep_latest() {
    for reverse in [false, true] {
        let mut replies = vec![
            json!({"id":6,"result":{"threadSettings":{"model":"first-model","effort":"low"}}}),
            json!({"id":7,"result":{"threadSettings":{"model":"latest-model","effort":"high"}}}),
        ];
        if reverse {
            replies.reverse();
        }
        let mut h = Harness::new(vec![replies]).await;
        h.select("plan").await.unwrap();
        h.backend
            .dispatch(Command::SetModel {
                model: "first-request".into(),
            })
            .await
            .unwrap();
        h.backend
            .dispatch(Command::SetModel {
                model: "latest-request".into(),
            })
            .await
            .unwrap();
        h.next().await;
        h.send("after confirmation").await.unwrap();
        let frame = h.frames().await.pop().unwrap();
        assert_eq!(
            frame["params"]["collaborationMode"]["settings"]["model"], "latest-model",
            "response order reversed={reverse}"
        );
        assert_eq!(
            frame["params"]["collaborationMode"]["settings"]["reasoning_effort"],
            "high"
        );
    }
}

#[tokio::test]
async fn codex_collaboration_receipt_before_newer_notification_preserves_later_setting() {
    let mut h = Harness::new(vec![vec![
        json!({"id":6,"result":{"threadSettings":{"model":"confirmed-model","effort":"low"}}}),
        settings("latest-model", json!("xhigh")),
    ]])
    .await;
    h.select("plan").await.unwrap();
    h.backend
        .dispatch(Command::SetModel {
            model: "requested-model".into(),
        })
        .await
        .unwrap();
    h.next().await;
    h.send("after notification").await.unwrap();
    assert_eq!(
        h.frames().await.pop().unwrap()["params"]["collaborationMode"]["settings"]["model"],
        "latest-model"
    );
}

#[tokio::test]
async fn codex_collaboration_replays_real_native_question_zero_id_roundtrip() {
    // Verbatim sanitized wire evidence from codex-0161-real-native-x9jy2_v3.
    let wire: Value = serde_json::from_str(include_str!("codex_native_question_fixture.json")).unwrap();
    let request = wire["native_request"].clone();
    let thread = request["params"]["threadId"].clone();
    let turn = request["params"]["turnId"].clone();
    let mut h = Harness::new(vec![
        vec![json!({"method":"thread/started","params":{"thread":{"id":thread}}})],
        vec![
            json!({"method":"turn/started","params":{"threadId":thread,"turn":{"id":turn}}}),
            request,
        ],
        vec![
            wire["resolved"][0].clone(),
            json!({"method":"turn/completed","params":{"threadId":thread,"turn":{"id":turn,"status":"completed"}}}),
        ],
    ])
    .await;
    h.next().await;
    h.select("plan").await.unwrap();
    h.send("fixture replay").await.unwrap();
    let mut events = h.backend.events();
    h.next().await;
    let pending = h.backend.pending_permission_requests();
    assert_eq!(pending.len(), 1);
    let questions = pending[0].questions.as_ref().unwrap();
    assert_eq!(questions[0]["id"], "color");
    assert_eq!(questions[0]["question"], "Choose Blue or Green.");
    assert_eq!(questions[0]["options"].as_array().unwrap().len(), 2);
    h.backend
        .dispatch(Command::AnswerAsk {
            request_id: pending[0].request_id.clone(),
            answers: Some(vec![super::super::types::QuestionAnswer {
                question_id: Some("color".into()),
                question: "Choose Blue or Green.".into(),
                labels: vec!["Blue".into()],
            }]),
        })
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let frames: Vec<Value> = String::from_utf8(h.captured.lock().await.clone())
                .unwrap()
                .lines()
                .filter_map(|s| serde_json::from_str(s).ok())
                .collect();
            if frames.contains(&wire["native_response"]) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    h.next().await;
    let mut resolved = 0;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while let Some(env) = events.next().await {
            match env.event {
                SessionEvent::AskResolved { .. } => resolved += 1,
                SessionEvent::PermissionResolved { .. } => panic!("native Ask must not resolve an approval"),
                SessionEvent::TurnResult { is_error, .. } => {
                    assert!(!is_error);
                    break;
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(resolved, 1, "answer plus server resolution must resolve only once");
    assert!(h.backend.pending_permission_requests().is_empty());
}

#[tokio::test]
async fn codex_collaboration_plan_is_independent_of_permissions() {
    let tail = format!(
        "{}\n{}\n{}\n",
        json!({"id":2,"result":{"thread":{"id":"th1"},"model":"actual-model","reasoningEffort":null}}),
        json!({"id":5,"result":catalog()}),
        json!({"method":"thread/settings/updated", "params":{"threadId":"th1", "threadSettings":{"model":"actual-model"}}})
    );
    let fake = FakeAgentIo::new(Vec::new(), None).with_gated_tail(tail.into_bytes());
    let release = fake.stdout_releaser();
    let backend = CodexSessionBackend::build_with_io("collaboration", Box::new(fake)).await;
    let mut events = backend.events();
    let permissions = backend.capabilities().available_modes;
    backend.run_handshake(HandshakeMode::Fresh).await.unwrap();
    release();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while let Some(env) = events.next().await {
            if matches!(env.event, SessionEvent::ConfigChanged { .. }) {
                break;
            }
        }
    })
    .await
    .unwrap();
    let result = backend
        .dispatch(Command::SetConfigOption {
            option_id: "collaboration_mode".into(),
            value: "plan".into(),
        })
        .await;
    assert!(
        result.is_ok(),
        "discovered Plan must be independently selectable: {result:?}"
    );
    assert_eq!(backend.capabilities().available_modes, permissions);
}
