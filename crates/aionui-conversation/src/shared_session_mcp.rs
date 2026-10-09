//! MCP servers every session gets from server-wide settings rather than from
//! its own conversation.
//!
//! Today that is the image generation server (see `ImageGenerationService` in
//! `aionui-system`). The conversation service asks a [`SharedSessionMcpSource`]
//! each time it assembles a session and merges the answer into the session's
//! MCP servers with [`merge_shared_session_mcp`].

use aionui_api_types::{IMAGE_GENERATION_MCP_NAME, SessionMcpServer};

/// Server-wide MCP servers every session gets (today: image generation).
///
/// Asked on every session assembly, so a changed setting reaches the next
/// session of any conversation without a restart. An empty answer means "none
/// right now", for example because the setting is off.
#[async_trait::async_trait]
pub trait SharedSessionMcpSource: Send + Sync {
    async fn shared_servers(&self) -> Vec<SessionMcpServer>;
}

/// Put `shared` into `servers`, replacing any entry with the same name (the
/// shared definition wins over a stale per-conversation snapshot).
///
/// [`IMAGE_GENERATION_MCP_NAME`] is reserved for the shared server: every entry
/// with that name is removed first, whether or not `shared` provides one. A
/// snapshot saved with the conversation, or an assistant's built-in selection
/// resolved when the conversation was created, would otherwise bring back the
/// credentials and model it captured then, long after the administrator
/// changed or switched off the shared setting.
pub fn merge_shared_session_mcp(servers: &mut Vec<SessionMcpServer>, shared: Vec<SessionMcpServer>) {
    servers.retain(|existing| existing.name != IMAGE_GENERATION_MCP_NAME);
    for server in shared {
        servers.retain(|existing| existing.name != server.name);
        servers.push(server);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use aionui_api_types::SessionMcpTransport;

    use super::*;

    /// A stdio server whose command tells the definitions apart.
    fn server(name: &str, command: &str) -> SessionMcpServer {
        SessionMcpServer {
            id: format!("id-{name}"),
            name: name.to_owned(),
            transport: SessionMcpTransport::Stdio {
                command: command.to_owned(),
                args: Vec::new(),
                env: HashMap::new(),
            },
        }
    }

    fn command(server: &SessionMcpServer) -> &str {
        match &server.transport {
            SessionMcpTransport::Stdio { command, .. } => command,
            other => panic!("stdio server expected, got {other:?}"),
        }
    }

    fn names(servers: &[SessionMcpServer]) -> Vec<&str> {
        servers.iter().map(|server| server.name.as_str()).collect()
    }

    #[test]
    fn shared_servers_are_appended_after_the_conversations_own() {
        let mut servers = vec![server("fs", "fs-cmd"), server("git", "git-cmd")];

        merge_shared_session_mcp(&mut servers, vec![server("aionui-image-generation", "node")]);

        assert_eq!(names(&servers), ["fs", "git", "aionui-image-generation"]);
    }

    #[test]
    fn nothing_shared_leaves_the_servers_as_they_were() {
        let mut servers = vec![server("fs", "fs-cmd"), server("git", "git-cmd")];
        let before = servers.clone();

        merge_shared_session_mcp(&mut servers, Vec::new());

        assert_eq!(servers, before);
    }

    #[test]
    fn nothing_shared_and_no_servers_stays_empty() {
        let mut servers = Vec::new();

        merge_shared_session_mcp(&mut servers, Vec::new());

        assert!(servers.is_empty());
    }

    #[test]
    fn a_shared_server_replaces_one_with_the_same_name() {
        let mut servers = vec![server("fs", "old-fs"), server("git", "git-cmd")];

        merge_shared_session_mcp(&mut servers, vec![server("fs", "shared-fs")]);

        assert_eq!(
            names(&servers),
            ["git", "fs"],
            "the shared definition takes the place at the end"
        );
        assert_eq!(command(&servers[1]), "shared-fs", "the shared definition wins");
    }

    #[test]
    fn a_stale_image_generation_server_is_replaced_by_the_shared_one() {
        let mut servers = vec![
            server("fs", "fs-cmd"),
            server("aionui-image-generation", "stale-node"),
            server("git", "git-cmd"),
        ];

        merge_shared_session_mcp(&mut servers, vec![server("aionui-image-generation", "shared-node")]);

        assert_eq!(names(&servers), ["fs", "git", "aionui-image-generation"]);
        assert_eq!(command(&servers[2]), "shared-node");
    }

    /// The shared setting being off must also remove what a snapshot kept.
    #[test]
    fn a_stale_image_generation_server_is_dropped_when_nothing_is_shared() {
        let mut servers = vec![server("fs", "fs-cmd"), server("aionui-image-generation", "stale-node")];

        merge_shared_session_mcp(&mut servers, Vec::new());

        assert_eq!(names(&servers), ["fs"]);
    }

    #[test]
    fn every_copy_of_the_reserved_name_is_dropped() {
        let mut servers = vec![
            server("aionui-image-generation", "first"),
            server("fs", "fs-cmd"),
            server("aionui-image-generation", "second"),
        ];

        merge_shared_session_mcp(&mut servers, vec![server("aionui-image-generation", "shared-node")]);

        assert_eq!(names(&servers), ["fs", "aionui-image-generation"]);
        assert_eq!(command(&servers[1]), "shared-node");
    }

    #[test]
    fn unrelated_servers_are_untouched_and_keep_their_order() {
        let mut servers = vec![
            server("zeta", "z"),
            server("alpha", "a"),
            server("aionui-image-generation", "stale-node"),
            server("mid", "m"),
        ];
        let unrelated: Vec<SessionMcpServer> = servers
            .iter()
            .filter(|server| server.name != "aionui-image-generation")
            .cloned()
            .collect();

        merge_shared_session_mcp(&mut servers, vec![server("aionui-image-generation", "shared-node")]);

        assert_eq!(servers[..3], unrelated[..]);
    }

    #[test]
    fn merging_the_same_shared_servers_again_changes_nothing() {
        let shared = vec![server("aionui-image-generation", "shared-node")];
        let mut servers = vec![server("fs", "fs-cmd")];
        merge_shared_session_mcp(&mut servers, shared.clone());
        let once = servers.clone();

        merge_shared_session_mcp(&mut servers, shared);

        assert_eq!(servers, once);
    }

    #[test]
    fn the_reserved_name_is_the_image_generation_server_name() {
        assert_eq!(IMAGE_GENERATION_MCP_NAME, "aionui-image-generation");
    }
}
