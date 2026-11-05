use std::collections::HashMap;

use aionui_api_types::{AskQuestionAnswer, ConfirmRequest};
use aionui_common::{Confirmation, now_ms};
use aionui_db::models::FeishuBindingRow;
use serde_json::Value;

use super::service::{outgoing, random_token};
use super::state::FeishuBridge;
use crate::error::ChannelError;
use crate::types::{ActionButton, UnifiedAction, UnifiedIncomingMessage};

const INTERACTION_TTL_MS: i64 = 30 * 60 * 1000;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct Question {
    text: String,
    options: Vec<String>,
    multiple: bool,
    selected: Vec<String>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(super) enum InteractionKind {
    Permission(Vec<(String, Value)>),
    Ask(Vec<Question>),
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct PendingInteraction {
    pub binding: FeishuBindingRow,
    pub request_id: String,
    pub msg_id: String,
    pub expires_at: i64,
    pub kind: InteractionKind,
    title: String,
    pub message_id: Option<String>,
}

impl PendingInteraction {
    fn matches(&self, binding: &FeishuBindingRow) -> bool {
        self.binding.id == binding.id && self.expires_at > now_ms()
    }

    fn card(&self, token: &str) -> crate::types::UnifiedOutgoingMessage {
        let mut text = self.title.clone();
        let mut buttons = Vec::new();
        match &self.kind {
            InteractionKind::Permission(options) => {
                for (index, (label, _)) in options.iter().enumerate() {
                    buttons.push(vec![button(label, "feishu.confirm", token, Some(index), None)]);
                }
            }
            InteractionKind::Ask(questions) => {
                for (qi, question) in questions.iter().enumerate() {
                    text.push_str(&format!("\n\n{}. {}", qi + 1, question.text));
                    if !question.selected.is_empty() {
                        text.push_str(&format!("\n已选：{}", question.selected.join("、")));
                    }
                    for (oi, option) in question.options.iter().enumerate() {
                        buttons.push(vec![button(
                            &format!("{} · {}", qi + 1, option),
                            "feishu.select",
                            token,
                            Some(oi),
                            Some(qi),
                        )]);
                    }
                }
                text.push_str(&format!(
                    "\n\n可点选后提交，或发送 /answer {token} 回答；多个问题用 | 分隔，多选答案用逗号分隔。"
                ));
                buttons.push(vec![
                    button("提交回答", "feishu.submit", token, None, None),
                    button("取消", "feishu.decline", token, None, None),
                ]);
            }
        }
        outgoing(&text, Some(buttons))
    }
}

fn button(label: &str, action: &str, token: &str, option: Option<usize>, question: Option<usize>) -> ActionButton {
    let mut params = HashMap::from([("token".into(), token.into())]);
    if let Some(option) = option {
        params.insert("option".into(), option.to_string());
    }
    if let Some(question) = question {
        params.insert("question".into(), question.to_string());
    }
    ActionButton {
        label: label.into(),
        action: action.into(),
        params: Some(params),
    }
}

fn questions_from_value(value: &Value) -> Vec<Question> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|question| {
            let text = question.get("question")?.as_str()?.to_owned();
            let options = question
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|option| option.get("label").and_then(Value::as_str).map(str::to_owned))
                .collect();
            Some(Question {
                text,
                options,
                multiple: question
                    .get("multi_select")
                    .or_else(|| question.get("multiSelect"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                selected: Vec::new(),
            })
        })
        .collect()
}

impl FeishuBridge {
    pub(super) async fn publish_question(
        &self,
        binding: &FeishuBindingRow,
        msg_id: &str,
        payload: &Value,
    ) -> Result<(), ChannelError> {
        let Some(request_id) = payload
            .get("request_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            return Ok(());
        };
        let questions = questions_from_value(&payload["questions"]);
        if questions.is_empty() {
            return Ok(());
        }
        self.register_interaction(PendingInteraction {
            binding: binding.clone(),
            request_id: request_id.into(),
            msg_id: msg_id.into(),
            expires_at: now_ms() + INTERACTION_TTL_MS,
            kind: InteractionKind::Ask(questions),
            title: "需要补充信息".into(),
            message_id: None,
        })
        .await
    }

    pub(super) async fn publish_confirmation(
        &self,
        binding: &FeishuBindingRow,
        msg_id: &str,
        confirmation: Confirmation,
    ) -> Result<(), ChannelError> {
        if confirmation.call_id.is_empty() {
            return Ok(());
        }
        let questions = confirmation
            .questions
            .as_ref()
            .map(questions_from_value)
            .unwrap_or_default();
        // Mirror AgentInstance's public contract: ACP/aionrs expose only the
        // permission channel; the direct-session path owns answer_ask.
        let dedicated_question_channel = matches!(
            self.deps.tasks.get_task(&binding.conversation_id),
            Some(aionui_ai_agent::AgentInstance::Session(_))
        );
        let kind = if questions.is_empty() || !dedicated_question_channel {
            InteractionKind::Permission(
                confirmation
                    .options
                    .into_iter()
                    .map(|option| (option.label, option.value))
                    .collect(),
            )
        } else {
            InteractionKind::Ask(questions)
        };
        if matches!(&kind, InteractionKind::Permission(options) if options.is_empty()) {
            return Ok(());
        }
        let mut title = format!(
            "{}\n{}",
            confirmation.title.as_deref().unwrap_or("需要操作确认"),
            confirmation.description
        );
        if !dedicated_question_channel && let Some(questions) = confirmation.questions.as_ref() {
            title.push_str(&format!("\n{}", questions));
        }
        self.register_interaction(PendingInteraction {
            binding: binding.clone(),
            request_id: confirmation.call_id,
            msg_id: msg_id.into(),
            expires_at: now_ms() + INTERACTION_TTL_MS,
            kind,
            title,
            message_id: None,
        })
        .await
    }

    async fn register_interaction(&self, pending: PendingInteraction) -> Result<(), ChannelError> {
        #[cfg(feature = "lark")]
        if self.deps.gateway.is_some() {
            self.restore_gateway_interactions().await?;
        }
        self.expire_interactions().await?;
        let mut interactions = self.interactions.lock().await;
        if interactions
            .values()
            .any(|current| current.binding.id == pending.binding.id && current.request_id == pending.request_id)
        {
            return Ok(());
        }
        let token = random_token()?;
        self.persist_interaction(&token, &pending).await?;
        interactions.insert(token, pending);
        Ok(())
    }

    pub(super) async fn flush_interactions(&self) {
        let unsent: Vec<_> = self
            .interactions
            .lock()
            .await
            .iter()
            .filter(|(_, pending)| pending.message_id.is_none() && pending.expires_at > now_ms())
            .map(|(token, pending)| (token.clone(), pending.clone()))
            .collect();
        for (token, pending) in unsent {
            let lock = self
                .conversation_locks
                .entry(pending.binding.conversation_id.clone())
                .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
                .clone();
            let _guard = lock.lock().await;
            if pending.expires_at <= now_ms() || !self.interactions.lock().await.contains_key(&token) {
                continue;
            }
            let current = self
                .deps
                .bindings
                .get_by_chat(
                    &pending.binding.app_id,
                    &pending.binding.tenant_key,
                    &pending.binding.chat_id,
                )
                .await
                .ok()
                .flatten();
            if current.as_ref().map(|row| row.id.as_str()) != Some(pending.binding.id.as_str()) {
                let _ = self.delete_persisted_interaction(&token).await;
                self.interactions.lock().await.remove(&token);
                continue;
            }
            let mut card = pending.card(&token);
            let mut parts = super::relay::split_text(card.text.as_deref().unwrap_or_default(), 3200);
            let last = parts.pop().unwrap_or_default();
            let mut prefixes_delivered = true;
            for (index, part) in parts.into_iter().enumerate() {
                if self
                    .send_bound_message(
                        &pending.binding,
                        outgoing(&part, None),
                        Some(&format!("{token}-prefix-{index}")),
                    )
                    .await
                    .is_err()
                {
                    prefixes_delivered = false;
                    break;
                }
            }
            if !prefixes_delivered {
                continue;
            }
            card.text = Some(last);
            if let Ok(id) = self.send_interaction_card(&pending, &token, card).await {
                let updated = {
                    let mut interactions = self.interactions.lock().await;
                    interactions.get_mut(&token).map(|current| {
                        current.message_id = Some(id);
                        current.clone()
                    })
                };
                if let Some(updated) = updated
                    && self.persist_interaction(&token, &updated).await.is_err()
                {
                    tracing::warn!(
                        code = "GATEWAY_INTERACTION_PERSIST_FAILED",
                        "feishu: card receipt will recover using its stable idempotency key"
                    );
                }
            }
        }
    }

    pub(super) async fn handle_interaction(
        &self,
        binding: &FeishuBindingRow,
        message: &UnifiedIncomingMessage,
        action: &UnifiedAction,
    ) -> Result<(), ChannelError> {
        let Some(params) = action.params.as_ref() else {
            return Ok(());
        };
        let Some(token) = params.get("token") else {
            return Ok(());
        };
        if action.action == "feishu.select" {
            let question = params.get("question").and_then(|value| value.parse::<usize>().ok());
            let option = params.get("option").and_then(|value| value.parse::<usize>().ok());
            let updated = {
                let mut interactions = self.interactions.lock().await;
                let Some(pending) = interactions.get_mut(token).filter(|pending| pending.matches(binding)) else {
                    drop(interactions);
                    return self.send_expired(binding).await;
                };
                if let (InteractionKind::Ask(questions), Some(qi), Some(oi)) = (&mut pending.kind, question, option)
                    && let Some(question) = questions.get_mut(qi)
                    && let Some(label) = question.options.get(oi).cloned()
                {
                    if question.multiple {
                        if question.selected.contains(&label) {
                            question.selected.retain(|current| current != &label);
                        } else {
                            question.selected.push(label);
                        }
                    } else {
                        question.selected = vec![label];
                    }
                }
                if pending.message_id.is_none() {
                    pending.message_id = message
                        .action
                        .as_ref()
                        .and_then(|action| action.context.message_id.clone());
                }
                pending.clone()
            };
            self.persist_interaction(token, &updated).await?;
            if let Some(message_id) = message
                .action
                .as_ref()
                .and_then(|action| action.context.message_id.as_deref())
            {
                self.edit_bound_message(binding, message_id, updated.card(token))
                    .await?;
            }
            return Ok(());
        }
        let (pending, response) = {
            let mut interactions = self.interactions.lock().await;
            let Some(pending) = interactions.get(token).filter(|pending| pending.matches(binding)) else {
                drop(interactions);
                return self.send_expired(binding).await;
            };
            let response = match (action.action.as_str(), &pending.kind) {
                ("feishu.confirm", InteractionKind::Permission(options)) => params
                    .get("option")
                    .and_then(|value| value.parse::<usize>().ok())
                    .and_then(|index| options.get(index))
                    .map(|(_, value)| InteractionResponse::Permission(value.clone())),
                ("feishu.decline", InteractionKind::Ask(_)) => Some(InteractionResponse::Ask(None)),
                ("feishu.submit", InteractionKind::Ask(questions))
                    if questions.iter().all(|question| !question.selected.is_empty()) =>
                {
                    Some(InteractionResponse::Ask(Some(
                        questions
                            .iter()
                            .map(|question| AskQuestionAnswer {
                                question: question.text.clone(),
                                labels: question.selected.clone(),
                            })
                            .collect(),
                    )))
                }
                _ => None,
            };
            let Some(response) = response else {
                drop(interactions);
                self.send_text(binding, "请先为每个问题选择答案，再提交。").await?;
                return Ok(());
            };
            let mut pending = interactions.remove(token).expect("checked above while holding mutex");
            if pending.message_id.is_none() {
                pending.message_id = message
                    .action
                    .as_ref()
                    .and_then(|action| action.context.message_id.clone());
            }
            (pending, response)
        };
        self.delete_persisted_interaction(token).await?;
        self.deliver_interaction(token, pending, response).await
    }

    pub(super) async fn handle_text_answer(
        &self,
        binding: &FeishuBindingRow,
        command: &str,
    ) -> Result<(), ChannelError> {
        let Some((token, text)) = command.split_once(char::is_whitespace) else {
            self.send_text(binding, "请使用 /answer 回答码 回答内容。").await?;
            return Ok(());
        };
        let (pending, answers) = {
            let mut interactions = self.interactions.lock().await;
            let Some(pending) = interactions.get(token).filter(|pending| pending.matches(binding)) else {
                drop(interactions);
                return self.send_expired(binding).await;
            };
            let InteractionKind::Ask(questions) = &pending.kind else {
                drop(interactions);
                return self.send_expired(binding).await;
            };
            let values: Vec<_> = text.split('|').map(str::trim).collect();
            if values.len() != questions.len() || values.iter().any(|value| value.is_empty()) {
                drop(interactions);
                self.send_text(binding, "请为每个问题提供回答，多个问题使用 | 分隔。")
                    .await?;
                return Ok(());
            }
            let answers: Vec<AskQuestionAnswer> = questions
                .iter()
                .zip(values)
                .map(|(question, value)| AskQuestionAnswer {
                    question: question.text.clone(),
                    labels: if question.multiple {
                        value
                            .split([',', '，'])
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .map(str::to_owned)
                            .collect()
                    } else {
                        vec![value.into()]
                    },
                })
                .collect();
            if answers.iter().any(|answer| answer.labels.is_empty()) {
                drop(interactions);
                self.send_text(binding, "每个问题都需要至少一个有效回答。").await?;
                return Ok(());
            }
            let pending = interactions.remove(token).expect("checked above while holding mutex");
            (pending, answers)
        };
        self.delete_persisted_interaction(token).await?;
        self.deliver_interaction(token, pending, InteractionResponse::Ask(Some(answers)))
            .await
    }

    async fn deliver_interaction(
        &self,
        token: &str,
        pending: PendingInteraction,
        response: InteractionResponse,
    ) -> Result<(), ChannelError> {
        let binding = &pending.binding;
        let lock = self
            .conversation_locks
            .entry(binding.conversation_id.clone())
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;
        let current = self
            .deps
            .bindings
            .get_by_chat(&binding.app_id, &binding.tenant_key, &binding.chat_id)
            .await?;
        if current.as_ref().map(|row| row.id.as_str()) != Some(binding.id.as_str()) {
            return self.send_expired(binding).await;
        }
        let result = match response {
            InteractionResponse::Permission(value) => {
                let pending_calls = self
                    .deps
                    .conversations
                    .list_confirmations(&binding.owner_user_id, &binding.conversation_id, &self.deps.tasks)
                    .await
                    .map_err(|error| ChannelError::MessageSendFailed(error.to_string()))?;
                if !pending_calls.iter().any(|call| call.call_id == pending.request_id) {
                    return self.send_expired(binding).await;
                }
                self.deps
                    .conversations
                    .confirm(
                        &binding.owner_user_id,
                        &binding.conversation_id,
                        &pending.request_id,
                        ConfirmRequest {
                            msg_id: pending.msg_id.clone(),
                            data: value,
                            always_allow: false,
                        },
                        &self.deps.tasks,
                    )
                    .await
            }
            InteractionResponse::Ask(answers) => {
                self.deps
                    .conversations
                    .answer_ask(
                        &binding.owner_user_id,
                        &binding.conversation_id,
                        &pending.request_id,
                        answers,
                        &self.deps.tasks,
                    )
                    .await
            }
        };
        match result {
            Ok(()) => {
                if let Some(message_id) = pending.message_id.as_deref() {
                    let _ = self
                        .edit_bound_message(binding, message_id, outgoing("该事项已处理。", None))
                        .await;
                }
                Ok(())
            }
            Err(error) => {
                self.persist_interaction(token, &pending).await?;
                self.interactions.lock().await.insert(token.into(), pending);
                Err(ChannelError::MessageSendFailed(error.to_string()))
            }
        }
    }

    async fn send_expired(&self, binding: &FeishuBindingRow) -> Result<(), ChannelError> {
        self.send_text(binding, "该事项已处理、已过期，或会话绑定已变更。")
            .await?;
        Ok(())
    }
}

impl FeishuBridge {
    async fn send_interaction_card(
        &self,
        pending: &PendingInteraction,
        token: &str,
        card: crate::types::UnifiedOutgoingMessage,
    ) -> Result<String, ChannelError> {
        #[cfg(feature = "lark")]
        if self.deps.gateway.is_some() {
            return self
                .send_gateway_message(
                    &pending.binding,
                    card,
                    Some(&format!("{token}-card")),
                    Some(crate::gateway::GatewayInteraction {
                        id: token.into(),
                        request_id: pending.request_id.clone(),
                        kind: if matches!(&pending.kind, InteractionKind::Ask(_)) {
                            "ask"
                        } else {
                            "permission"
                        }
                        .into(),
                        expires_at: pending.expires_at,
                    }),
                )
                .await;
        }
        self.send_bound_message(&pending.binding, card, Some(&format!("{token}-card")))
            .await
    }

    async fn persist_interaction(&self, token: &str, pending: &PendingInteraction) -> Result<(), ChannelError> {
        let _ = (token, pending);
        #[cfg(feature = "lark")]
        if self.deps.gateway.is_some() {
            self.gateway_repository()?
                .save_interaction(
                    token,
                    &pending.binding.id,
                    &serde_json::to_string(pending)?,
                    pending.expires_at,
                )
                .await?;
        }
        Ok(())
    }

    async fn delete_persisted_interaction(&self, token: &str) -> Result<(), ChannelError> {
        let _ = token;
        #[cfg(feature = "lark")]
        if self.deps.gateway.is_some() {
            self.gateway_repository()?.delete_interaction(token).await?;
        }
        Ok(())
    }

    pub(super) async fn discard_conversation_interactions(
        &self,
        conversation: &str,
        request: Option<&str>,
    ) -> Result<(), ChannelError> {
        let tokens: Vec<_> = self
            .interactions
            .lock()
            .await
            .iter()
            .filter(|(_, pending)| {
                pending.binding.conversation_id == conversation
                    && request.is_none_or(|request| pending.request_id == request)
            })
            .map(|(token, _)| token.clone())
            .collect();
        for token in tokens {
            self.delete_persisted_interaction(&token).await?;
            self.interactions.lock().await.remove(&token);
        }
        Ok(())
    }

    pub(super) async fn expire_interactions(&self) -> Result<(), ChannelError> {
        let tokens: Vec<_> = self
            .interactions
            .lock()
            .await
            .iter()
            .filter(|(_, pending)| pending.expires_at <= now_ms())
            .map(|(token, _)| token.clone())
            .collect();
        for token in tokens {
            self.delete_persisted_interaction(&token).await?;
            self.interactions.lock().await.remove(&token);
        }
        Ok(())
    }

    #[cfg(feature = "lark")]
    pub(super) async fn discard_gateway_interaction(&self, token: &str) -> Result<(), ChannelError> {
        self.delete_persisted_interaction(token).await?;
        self.interactions.lock().await.remove(token);
        Ok(())
    }

    #[cfg(feature = "lark")]
    pub(super) async fn restore_gateway_interactions(&self) -> Result<(), ChannelError> {
        let mut restored = self.gateway_restored.lock().await;
        if *restored {
            return Ok(());
        }
        for row in self.gateway_repository()?.list_interactions(now_ms()).await? {
            let pending: PendingInteraction = serde_json::from_str(&row.payload)?;
            let current = self
                .deps
                .bindings
                .get_by_conversation(&pending.binding.owner_user_id, &pending.binding.conversation_id)
                .await?;
            if pending.binding.id != row.binding_id
                || pending.expires_at != row.expires_at
                || current.as_ref().map(|binding| &binding.id) != Some(&row.binding_id)
            {
                self.gateway_repository()?.delete_interaction(&row.token).await?;
                continue;
            }
            self.interactions.lock().await.entry(row.token).or_insert(pending);
        }
        *restored = true;
        Ok(())
    }
}

enum InteractionResponse {
    Permission(Value),
    Ask(Option<Vec<AskQuestionAnswer>>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn questions_preserve_text_and_multi_select_contract() {
        let parsed = questions_from_value(&serde_json::json!([
            {"question":"Choose targets", "multi_select":true, "options":[{"label":"One"},{"label":"Two"}]},
            {"question":"Describe", "options":[]}
        ]));
        assert_eq!(parsed.len(), 2);
        assert!(parsed[0].multiple);
        assert_eq!(parsed[0].options, ["One", "Two"]);
        assert_eq!(parsed[1].text, "Describe");
    }
}
