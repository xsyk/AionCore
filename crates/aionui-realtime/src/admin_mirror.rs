//! Which user-scoped events are also delivered to the super admin.
//!
//! The super admin's sider shows every user's conversations and can continue
//! them, so it must receive their live conversation traffic (streaming
//! replies, list changes, turn lifecycle). Everything else a user receives —
//! settings, skills, MCP, extensions — stays private to that user.

use aionui_api_types::WebSocketMessage;

const CONVERSATION_EVENT_PREFIXES: [&str; 3] = ["message.", "conversation.", "turn."];

/// Whether a user-scoped event concerns a conversation and should be mirrored
/// to the super admin.
pub fn is_conversation_scoped_event(event: &WebSocketMessage<serde_json::Value>) -> bool {
    event.data.get("conversation_id").is_some()
        || CONVERSATION_EVENT_PREFIXES
            .iter()
            .any(|prefix| event.name.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(name: &str, data: serde_json::Value) -> WebSocketMessage<serde_json::Value> {
        WebSocketMessage::new(name, data)
    }

    #[test]
    fn conversation_events_are_mirrored() {
        assert!(is_conversation_scoped_event(&event(
            "message.stream",
            json!({"user_id": "u"})
        )));
        assert!(is_conversation_scoped_event(&event(
            "conversation.listChanged",
            json!({})
        )));
        assert!(is_conversation_scoped_event(&event("turn.completed", json!({}))));
        assert!(is_conversation_scoped_event(&event(
            "skills.loaded",
            json!({"conversation_id": "c1"})
        )));
    }

    #[test]
    fn private_user_events_are_not_mirrored() {
        assert!(!is_conversation_scoped_event(&event(
            "settings.changed",
            json!({"user_id": "u"})
        )));
        assert!(!is_conversation_scoped_event(&event("mcp.serversChanged", json!({}))));
        assert!(!is_conversation_scoped_event(&event(
            "extensions.stateChanged",
            json!({})
        )));
    }
}
