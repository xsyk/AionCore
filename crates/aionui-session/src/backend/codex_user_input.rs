//! Codex requestUserInput contract mapping. Verified against
//! docs/superpowers/sdd/1.0.3/protocol-captures/codex-0.161.0-214-20261106/
//! schema-full/{ToolRequestUserInputParams,ToolRequestUserInputResponse}.json.
//! These are schema contracts; live CLI timing/cancellation is not verified.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast};

use super::super::types::{BackendError, CommandReceipt, PendingPermissionView, QuestionAnswer};
use super::{Admission, CodexSessionBackend, SessionEnvelope, SessionEvent, emit, write_reverse_error};

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InputOption {
    label: String,
    description: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InputQuestion {
    id: String,
    header: String,
    question: String,
    #[serde(default)]
    is_other: bool,
    #[serde(default)]
    is_secret: bool,
    options: Option<Vec<InputOption>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InputParams {
    is_blocking: bool,
    thread_id: String,
    turn_id: String,
    #[serde(rename = "itemId")]
    _item_id: String,
    questions: Vec<InputQuestion>,
}

#[derive(Clone)]
struct PendingUserInput {
    wire_id: Value,
    questions: Vec<InputQuestion>,
}

struct UserInputEntry {
    thread_id: String,
    turn_id: String,
    turn_gen: u64,
    pending: Option<PendingUserInput>,
}

struct InputScope {
    thread_id: String,
    turn_id: Option<String>,
    turn_gen: u64,
}

struct InputRegistry {
    entries: HashMap<String, UserInputEntry>,
    scope: Option<InputScope>,
    closed: bool,
    process_namespace: uuid::Uuid,
}

impl Default for InputRegistry {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            scope: None,
            closed: false,
            process_namespace: uuid::Uuid::new_v4(),
        }
    }
}

impl InputRegistry {
    fn request_id(&self, wire_id: &Value) -> String {
        // A fresh namespace also isolates entirely new backends/service restarts.
        // JSON formatting keeps numeric and string IDs distinct; replies retain
        // the original wire Value separately. Never accept a bare wire ID as a card.
        format!("codex-input:{}:{wire_id}", self.process_namespace)
    }

    fn accepts(&self, thread_id: &str, turn_id: &str, turn_gen: u64) -> bool {
        !self.closed
            && self.scope.as_ref().is_none_or(|scope| {
                scope.thread_id == thread_id
                    && scope
                        .turn_id
                        .as_ref()
                        .is_none_or(|id| id == turn_id && scope.turn_gen == turn_gen)
            })
    }

    fn retire_pending(&mut self) -> usize {
        self.entries
            .values_mut()
            .filter_map(|entry| entry.pending.take())
            .count()
    }
}

/// Completed/rejected entries retain only identity until process replacement,
/// so late resolved notifications cannot decrement an unrelated approval counter.
#[derive(Default)]
pub(super) struct UserInputs {
    registry: std::sync::Mutex<InputRegistry>,
    // Shared by registration, answers, resolved and lifecycle changes. Never hold
    // the sync registry lock across IO. Failed/cancelled writes preserve pending.
    gate: Mutex<()>,
}

impl PendingUserInput {
    fn questions_value(&self) -> Value {
        Value::Array(self.questions.iter().map(|q| json!({
            "id": q.id, "question": q.question, "header": q.header,
            "options": q.options.as_deref().unwrap_or_default().iter()
                .map(|option| json!({"label":option.label,"description":option.description})).collect::<Vec<_>>(),
            "multiSelect": false, "is_other": q.is_other, "is_secret": q.is_secret,
        })).collect())
    }

    fn response(&self, answers: &[QuestionAnswer]) -> Result<Value, BackendError> {
        let invalid =
            || BackendError::Transport("invalid user input answers: supply every question exactly once".into());
        if answers.len() != self.questions.len() {
            return Err(invalid());
        }
        let mut used = HashSet::new();
        let mut result = serde_json::Map::new();
        for answer in answers {
            // Explicit IDs are authoritative. An unknown explicit ID must not fall
            // back to text. Legacy clients retain ambiguity guards and text order.
            let question = if let Some(id) = &answer.question_id {
                self.questions.iter().find(|q| &q.id == id)
            } else if let Some(q) = self.questions.iter().find(|q| q.id == answer.question) {
                if self
                    .questions
                    .iter()
                    .any(|other| other.id != q.id && other.question == answer.question)
                {
                    return Err(BackendError::Transport("ambiguous user input question identity".into()));
                }
                Some(q)
            } else {
                self.questions
                    .iter()
                    .find(|q| q.question == answer.question && !used.contains(&q.id))
            }
            .ok_or_else(invalid)?;
            if !used.insert(question.id.clone()) || answer.labels.len() != 1 || answer.labels[0].trim().is_empty() {
                return Err(invalid());
            }
            result.insert(question.id.clone(), json!({"answers": answer.labels}));
        }
        Ok(json!({"jsonrpc":"2.0", "id":self.wire_id, "result":{"answers":result}}))
    }
}

impl UserInputs {
    pub(super) fn pending(&self) -> Vec<PendingPermissionView> {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .iter()
            .filter_map(|(id, entry)| {
                entry.pending.as_ref().map(|pending| PendingPermissionView {
                    request_id: id.clone(),
                    tool_name: "RequestUserInput".into(),
                    questions: Some(pending.questions_value()),
                })
            })
            .collect()
    }

    pub(super) async fn bind_scope(&self, thread_id: &str, turn_id: Option<&str>, turn_gen: &AtomicU64) {
        let _gate = self.gate.lock().await;
        let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        // A repeated thread/started must not erase the more precise turn binding.
        if turn_id.is_none()
            && registry
                .scope
                .as_ref()
                .is_some_and(|scope| scope.thread_id == thread_id)
        {
            return;
        }
        let turn_gen = turn_gen.load(Ordering::SeqCst);
        let retired = registry
            .entries
            .values_mut()
            .filter(|entry| {
                entry.thread_id != thread_id
                    || turn_id.is_some_and(|id| entry.turn_id != id || entry.turn_gen != turn_gen)
            })
            .filter_map(|entry| entry.pending.take())
            .count();
        registry.scope = Some(InputScope {
            thread_id: thread_id.into(),
            turn_id: turn_id.map(str::to_owned),
            turn_gen,
        });
        registry.closed = false;
        if retired > 0 {
            tracing::info!(retired, turn_gen, "codex user inputs retired on scope change");
        }
    }

    pub(super) async fn retire(&self, reason: &'static str) {
        let _gate = self.gate.lock().await;
        let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        let retired = registry.retire_pending();
        registry.closed = true;
        if retired > 0 {
            tracing::info!(retired, reason, "codex user inputs retired");
        }
    }

    pub(super) async fn reset_process(&self) {
        let _gate = self.gate.lock().await;
        let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        registry.entries.clear();
        registry.scope = None;
        registry.closed = false;
        registry.process_namespace = uuid::Uuid::new_v4();
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn register(
        &self,
        frame: &Value,
        stdin: &Arc<Mutex<Option<aionui_process::BoxedStdin>>>,
        event_tx: &broadcast::Sender<SessionEnvelope>,
        session_id: &str,
        turn_gen: &AtomicU64,
    ) {
        let _gate = self.gate.lock().await;
        let turn_gen = turn_gen.load(Ordering::SeqCst);
        let Some(wire_id) = frame.get("id").filter(|id| id.is_string() || id.as_i64().is_some()) else {
            write_reverse_error(stdin, &frame["id"], -32600, "invalid requestUserInput request id").await;
            return;
        };
        let request_id = {
            let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            let request_id = registry.request_id(wire_id);
            if registry.entries.contains_key(&request_id) {
                return;
            }
            for entry in registry.entries.values_mut() {
                if entry.turn_gen != turn_gen {
                    entry.pending = None;
                }
            }
            registry.entries.insert(
                request_id.clone(),
                UserInputEntry {
                    thread_id: frame["params"]["threadId"].as_str().unwrap_or_default().into(),
                    turn_id: frame["params"]["turnId"].as_str().unwrap_or_default().into(),
                    turn_gen,
                    pending: None,
                },
            );
            request_id
        };
        let parsed = serde_json::from_value::<InputParams>(frame["params"].clone());
        let result = parsed
            .map_err(|_| "invalid requestUserInput params")
            .and_then(|params| {
                if !params.is_blocking {
                    return Err("nonblocking requestUserInput is unsupported");
                }
                if params.questions.iter().any(|q| q.is_secret) {
                    // Existing web/IM clients have no reliable secret transport.
                    return Err("secret requestUserInput is unsupported");
                }
                let mut ids = HashSet::new();
                if params.thread_id.is_empty()
                    || params.turn_id.is_empty()
                    || params.questions.is_empty()
                    || params
                        .questions
                        .iter()
                        .any(|q| q.id.is_empty() || q.question.trim().is_empty() || !ids.insert(q.id.clone()))
                {
                    return Err("invalid requestUserInput question identities");
                }
                if !self.registry.lock().unwrap_or_else(|e| e.into_inner()).accepts(
                    &params.thread_id,
                    &params.turn_id,
                    turn_gen,
                ) {
                    return Err("requestUserInput does not belong to the active turn");
                }
                Ok(params)
            });
        let params = match result {
            Ok(params) => params,
            Err(message) => {
                tracing::warn!(session_id, request_id, reason = message, "codex user input rejected");
                write_reverse_error(stdin, wire_id, -32602, message).await;
                return;
            }
        };
        let pending = PendingUserInput {
            wire_id: wire_id.clone(),
            questions: params.questions,
        };
        let questions = pending.questions_value();
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .get_mut(&request_id)
            .expect("registered input")
            .pending = Some(pending);
        tracing::info!(
            session_id,
            request_id,
            thread_id = params.thread_id,
            turn_id = params.turn_id,
            turn_gen,
            "codex user input opened"
        );
        emit(
            event_tx,
            session_id,
            turn_gen,
            SessionEvent::Ask { request_id, questions },
        );
    }

    /// Known identities, including retired asks and mismatched threads, must
    /// never fall through to PermissionResolved. Only a current pending Ask emits.
    pub(super) async fn resolve(
        &self,
        params: &Value,
        event_tx: &broadcast::Sender<SessionEnvelope>,
        session_id: &str,
        turn_gen: &AtomicU64,
    ) -> bool {
        let Some(id) = params.get("requestId").or_else(|| params.get("request_id")) else {
            return false;
        };
        let _gate = self.gate.lock().await;
        let current_gen = turn_gen.load(Ordering::SeqCst);
        let (request_id, resolved_gen) = {
            let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            let request_id = registry.request_id(id);
            let Some(entry) = registry.entries.get(&request_id) else {
                return false;
            };
            if params["threadId"].as_str() != Some(entry.thread_id.as_str()) {
                return true;
            }
            let current =
                entry.turn_gen == current_gen && registry.accepts(&entry.thread_id, &entry.turn_id, entry.turn_gen);
            let entry = registry.entries.get_mut(&request_id).expect("known input");
            let resolved_gen = entry.pending.take().filter(|_| current).map(|_| entry.turn_gen);
            (request_id, resolved_gen)
        };
        if let Some(turn_gen) = resolved_gen {
            tracing::info!(session_id, request_id, turn_gen, "codex user input resolved by server");
            emit(event_tx, session_id, turn_gen, SessionEvent::AskResolved { request_id });
        }
        true
    }
}

impl CodexSessionBackend {
    pub(super) async fn answer_user_input(
        &self,
        request_id: &str,
        answers: Option<Vec<QuestionAnswer>>,
    ) -> Result<CommandReceipt, BackendError> {
        let _gate = self.pending_user_inputs.gate.lock().await;
        let no_pending = || BackendError::Transport("no pending user input for request id".into());
        let (pending, turn_gen) = {
            let mut registry = self
                .pending_user_inputs
                .registry
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let entry = registry.entries.get(request_id).ok_or_else(no_pending)?;
            let current = entry.turn_gen == self.turn_gen.load(Ordering::SeqCst)
                && registry.accepts(&entry.thread_id, &entry.turn_id, entry.turn_gen);
            let entry = registry.entries.get_mut(request_id).expect("known input");
            if !current {
                entry.pending = None;
                return Err(no_pending());
            }
            (entry.pending.clone().ok_or_else(no_pending)?, entry.turn_gen)
        };
        // The response schema defines no cancellation shape. Unsupported dismissal
        // and failed/cancelled writes preserve the pending entry for a retry.
        let answers =
            answers.ok_or_else(|| BackendError::Transport("requestUserInput cancellation is unsupported".into()))?;
        let response = pending.response(&answers)?;
        self.write_frame(response).await?;
        self.pending_user_inputs
            .registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .get_mut(request_id)
            .expect("input protected by gate")
            .pending = None;
        // A concurrent admission can advance the atomic while IO is suspended.
        // Never lower an old answer as a decrement of the new turn's counter.
        if self.turn_gen.load(Ordering::SeqCst) != turn_gen {
            return Err(no_pending());
        }
        tracing::info!(
            session_id = self.session_id,
            request_id,
            turn_gen,
            "codex user input answer written"
        );
        emit(
            &self.event_tx,
            &self.session_id,
            turn_gen,
            SessionEvent::AskResolved {
                request_id: request_id.into(),
            },
        );
        Ok(CommandReceipt {
            accepted: true,
            admission: Admission::NoTurn,
            turn_gen,
        })
    }
}
