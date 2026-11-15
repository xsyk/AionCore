//! Independent next-turn collaboration selection. Preset model/effort are
//! metadata, never user settings (0.161.0 CollaborationModeListResponse schema).
use serde_json::{Value, json};
use std::collections::HashMap;

use super::BackendError;
use crate::capability::ModeInfo;
use crate::event::SessionEvent;

#[derive(Default)]
pub(super) struct Collaboration {
    pub modes: Vec<ModeInfo>,
    pub selected: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub process_epoch: u64,
    pub startup_error: Option<i64>,
    // Only notifications advance this revision. Ordered RPC receipts must not
    // invalidate later requests registered before the first receipt arrived.
    revision: u64,
    last_confirmed_rpc: Option<u64>,
    threads: HashMap<u64, (u64, bool)>,
    settings: HashMap<u64, PendingSettings>,
    readback: Option<SettingsReadback>,
}

struct PendingSettings {
    revision: u64,
    acknowledged: bool,
    observed: bool,
    expected: Value,
}

struct SettingsReadback {
    id: u64,
    revision: u64,
    through: u64,
    thread_id: String,
    outcome: Option<bool>,
}

impl Collaboration {
    /// A replacement process retains only the user's token, never old settings.
    pub fn reset_process(&mut self) {
        self.process_epoch += 1;
        self.model = None;
        self.effort = None;
        self.startup_error = None;
        self.last_confirmed_rpc = None;
        self.threads.clear();
        self.settings.clear();
        self.readback = None;
        self.revision += 1;
    }

    pub fn register_thread(&mut self, id: u64, fresh: bool) {
        self.threads.insert(id, (self.revision, fresh));
    }

    /// Returns whether this was a correlated startup response. Only actual
    /// response fields may confirm settings; never copy a requested value.
    pub fn thread_response(&mut self, id: u64, frame: &Value) -> bool {
        let Some((revision, fresh)) = self.threads.remove(&id) else {
            return false;
        };
        if let Some(error) = frame.get("error") {
            if fresh {
                self.startup_error = Some(error.get("code").and_then(Value::as_i64).unwrap_or(-32603));
            }
        } else if revision == self.revision
            && self.last_confirmed_rpc.is_none_or(|last| id > last)
            && let Some(result) = frame.get("result")
        {
            self.confirm(result, true);
            self.last_confirmed_rpc = Some(id);
        }
        true
    }

    pub fn register_settings(&mut self, id: u64, expected: Value) {
        self.settings.insert(
            id,
            PendingSettings {
                revision: self.revision,
                acknowledged: false,
                observed: false,
                expected,
            },
        );
    }

    pub fn forget_settings(&mut self, id: u64) {
        self.settings.remove(&id);
    }

    pub fn settings_response(&mut self, id: u64, frame: &Value) {
        let Some(pending) = self.settings.get_mut(&id) else {
            return;
        };
        let revision = pending.revision;
        if frame.get("error").is_some() || pending.observed || self.last_confirmed_rpc.is_some_and(|last| id <= last) {
            self.settings.remove(&id);
        } else if let Some(settings) = frame.get("result").and_then(|r| r.get("threadSettings")) {
            // Preserve the existing nonempty-receipt path (current 0.161.0
            // captures return {}). An unrelated newer notification cannot retire
            // this request or be overwritten by its stale receipt.
            if revision == self.revision && self.last_confirmed_rpc.is_none_or(|last| id > last) {
                self.confirm(settings, false);
                self.last_confirmed_rpc = Some(id);
                self.settings.remove(&id);
            } else {
                pending.acknowledged = true;
            }
        } else {
            // 0.161.0 success is {}: delivery alone is not observation.
            pending.acknowledged = true;
        }
    }

    pub fn notification(&mut self, settings: &Value) {
        self.confirm(settings, false);
        self.revision += 1;
        self.settings.retain(|_, pending| {
            let matches = pending.expected.as_object().is_some_and(|expected| {
                !expected.is_empty()
                    && expected.iter().all(|(key, value)| {
                        let actual = match key.as_str() {
                            "permissions" => settings.pointer("/activePermissionProfile/id"),
                            "effort" => settings
                                .get("effort")
                                .or_else(|| settings.pointer("/collaborationMode/settings/reasoning_effort")),
                            _ => settings.get(key),
                        };
                        actual == Some(value)
                    })
            });
            pending.observed |= matches;
            !(pending.acknowledged && pending.observed)
        });
    }

    /// Called under dispatch_boundary, after every covered write was acknowledged.
    /// A readback may retire superseded/coalesced requests without adopting their
    /// requested values. Late receipts cannot modify a completed or expired read.
    pub fn can_readback(&self) -> bool {
        !self.settings.is_empty() && self.settings.values().all(|p| p.acknowledged)
    }

    pub fn begin_readback(&mut self, id: u64, thread_id: &str) -> bool {
        if !self.can_readback() {
            return false;
        }
        self.readback = Some(SettingsReadback {
            id,
            revision: self.revision,
            through: *self.settings.keys().max().unwrap(),
            thread_id: thread_id.into(),
            outcome: None,
        });
        true
    }

    pub fn readback_response(&mut self, id: u64, frame: &Value) -> bool {
        let Some(read) = self.readback.as_mut().filter(|r| r.id == id) else {
            return false;
        };
        let thread = frame.pointer("/result/thread");
        // Verified on loaded, idle ephemeral threads in r3-settings-readback-evidence.json.
        // Missing/unloaded metadata cannot confirm the next-turn settings.
        let valid = read.revision == self.revision
            && thread.is_some_and(|t| {
                t.get("id").and_then(Value::as_str) == Some(read.thread_id.as_str())
                    && t.pointer("/status/type").and_then(Value::as_str) == Some("idle")
                    && t.get("model").and_then(Value::as_str).is_some_and(|s| !s.is_empty())
                    && t.get("reasoningEffort").is_some_and(|e| e.is_null() || e.is_string())
            });
        read.outcome = Some(valid);
        if valid {
            let through = read.through;
            self.confirm(thread.unwrap(), true);
            self.settings.retain(|id, p| *id > through || !p.acknowledged);
        }
        valid
    }

    pub fn readback_outcome(&self, id: u64) -> Option<bool> {
        self.readback.as_ref().filter(|r| r.id == id).and_then(|r| r.outcome)
    }

    pub fn forget_readback(&mut self, id: u64) {
        if self.readback.as_ref().is_some_and(|r| r.id == id) {
            self.readback = None;
        }
    }

    fn confirm(&mut self, settings: &Value, thread_response: bool) {
        if let Some(model) = settings.get("model").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            self.model = Some(model.into());
        }
        let effort = if thread_response {
            settings.get("reasoningEffort")
        } else {
            settings
                .get("effort")
                .or_else(|| settings.pointer("/collaborationMode/settings/reasoning_effort"))
        };
        if let Some(effort) = effort {
            self.effort = effort.as_str().map(str::to_owned);
        }
    }

    pub fn pending(&self) -> bool {
        !self.settings.is_empty()
    }

    pub fn awaiting_thread(&self) -> bool {
        self.selected.is_some() && !self.threads.is_empty()
    }

    pub fn turn_value(&self) -> Result<Option<Value>, BackendError> {
        let Some(mode) = self.selected.as_deref() else {
            return Ok(None);
        };
        if self.pending() {
            return Err(BackendError::Transport("codex settings pending confirmation".into()));
        }
        let model = self
            .model
            .as_deref()
            .ok_or_else(|| BackendError::Transport("codex model not confirmed".into()))?;
        Ok(Some(
            json!({"mode":mode, "settings":{"model":model,"reasoning_effort":self.effort,"developer_instructions":null}}),
        ))
    }

    pub fn discover(&mut self, result: &Value) {
        self.modes = result
            .get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let mode = entry.get("mode")?.as_str()?;
                if !matches!(mode, "plan" | "default") {
                    return None;
                }
                Some(ModeInfo {
                    id: mode.into(),
                    name: entry.get("name")?.as_str()?.into(),
                    description: None,
                })
            })
            .collect();
    }

    pub fn select(&mut self, value: String) -> Result<(), BackendError> {
        if !self.modes.iter().any(|m| m.id == value) {
            return Err(BackendError::Transport("codex collaboration_mode unavailable".into()));
        }
        self.selected = Some(value);
        Ok(())
    }

    pub fn event(&self) -> SessionEvent {
        SessionEvent::CollaborationModeUpdated {
            modes: self.modes.clone(),
            selected_mode: self.selected.clone(),
        }
    }
}
