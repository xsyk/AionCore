use std::{
    collections::HashSet,
    path::{Component, Path, PathBuf},
};

use percent_encoding::percent_decode_str;
use pulldown_cmark::{Event, Parser, Tag};
use serde_json::Value;

pub(super) fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && Path::new(value).components().count() == 1
        && matches!(Path::new(value).components().next(), Some(Component::Normal(_)))
}

fn local_path(value: &str) -> Option<&Path> {
    let value = value
        .strip_prefix("file://")
        .or_else(|| value.strip_prefix("sandbox:"))
        .unwrap_or(value);
    if value.is_empty()
        || value.contains('\0')
        || value.contains("://")
        || value.starts_with("data:")
        || value.starts_with("javascript:")
    {
        return None;
    }
    let path = Path::new(value);
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return None;
    }
    Some(path)
}

/// The HTTP query extractor has already decoded its transport encoding. Raw
/// attachment paths are filesystem names, so neither is URI-decoded here.
pub(super) fn resource_path(value: &str, workspace: Option<&str>) -> Option<PathBuf> {
    let path = local_path(value)?;
    if path.is_absolute() {
        Some(path.to_owned())
    } else {
        workspace
            .filter(|root| !root.is_empty())
            .map(|root| Path::new(root).join(path))
    }
}

/// A visible tool's input parameters, permission request, location or diff path
/// are not evidence that its file contents were shared. Keep the grant sources
/// explicit and tied to the persisted message schema instead of walking JSON.
pub(super) fn referenced_paths(message_type: &str, value: &Value) -> HashSet<String> {
    let mut paths = HashSet::new();
    match message_type {
        "text" => {
            collect_files(value.get("files"), &mut paths);
            if let Some(text) = value.get("content").and_then(Value::as_str) {
                collect_markdown(text, &mut paths);
                if let Some((_, attachments)) = text.rsplit_once("[[AION_FILES]]") {
                    for path in attachments.lines().map(str::trim).filter(|line| !line.is_empty()) {
                        insert_literal(path, &mut paths);
                    }
                }
            }
        }
        "acp_tool_call" => {
            if let Some(update) = value
                .get("update")
                .filter(|update| update.get("status").and_then(Value::as_str) == Some("completed"))
            {
                for key in ["rawOutput", "raw_output"] {
                    if let Some(output) = update.get(key) {
                        collect_image_output(output, &mut paths);
                        collect_output_resources(output, &mut paths);
                    }
                }
                if let Some(content) = update.get("content") {
                    collect_output_resources(content, &mut paths);
                }
            }
        }
        "tool_call" if value.get("status").and_then(Value::as_str) == Some("completed") => {
            if let Some(output) = value.get("output") {
                if let Some(text) = output.as_str() {
                    if let Ok(output) = serde_json::from_str::<Value>(text) {
                        collect_output_resources(&output, &mut paths);
                    }
                } else {
                    collect_output_resources(output, &mut paths);
                }
            }
        }
        "tool_group" => {
            for entry in value
                .as_array()
                .into_iter()
                .flatten()
                .filter(|entry| entry.get("status").and_then(Value::as_str) == Some("Success"))
            {
                if let Some(output) = entry
                    .get("result_display")
                    .filter(|output| output.get("img_url").is_some())
                {
                    if let Some(path) = output
                        .get("relative_path")
                        .and_then(Value::as_str)
                        .filter(|path| !path.is_empty())
                    {
                        insert_literal(path, &mut paths);
                    } else if let Some(uri) = output.get("img_url").and_then(Value::as_str) {
                        insert_uri(uri, &mut paths);
                    }
                }
            }
        }
        _ => {}
    }
    paths
}

fn collect_files(value: Option<&Value>, paths: &mut HashSet<String>) {
    for path in value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        insert_literal(path, paths);
    }
}

fn collect_markdown(text: &str, paths: &mut HashSet<String>) {
    for event in Parser::new(text) {
        if let Event::Start(Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. }) = event {
            insert_uri(&dest_url, paths);
        }
    }
}

fn insert_literal(path: &str, paths: &mut HashSet<String>) {
    if local_path(path).is_some() {
        paths.insert(path.to_owned());
    }
}

fn insert_uri(uri: &str, paths: &mut HashSet<String>) {
    // Markdown destinations and structured resource URIs have URI semantics.
    // Decode exactly here, once, then apply traversal/scheme checks. A remaining
    // percent escape is a literal filename and must never be decoded again.
    if let Ok(decoded) = percent_decode_str(uri).decode_utf8() {
        insert_literal(&decoded, paths);
    }
}

fn collect_image_output(output: &Value, paths: &mut HashSet<String>) {
    if let Some(path) = output
        .get("image")
        .and_then(|image| image.get("path"))
        .and_then(Value::as_str)
    {
        insert_literal(path, paths);
    } else if let Some(path) = output.get("saved_path").and_then(Value::as_str) {
        let image_path = Path::new(path)
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| {
                matches!(
                    ext.to_ascii_lowercase().as_str(),
                    "png" | "jpg" | "jpeg" | "webp" | "gif"
                )
            });
        if image_path || output.get("result_omitted_reason").and_then(Value::as_str) == Some("image_base64") {
            insert_literal(path, paths);
        }
    }
}

/// Recognize resource objects only inside a completed tool's output container.
/// In particular, a generic object with a `path`/`url` key grants nothing.
fn collect_output_resources(output: &Value, paths: &mut HashSet<String>) {
    match output {
        Value::Array(items) => {
            for item in items {
                collect_output_resources(item, paths);
            }
        }
        Value::Object(object) => match object.get("type").and_then(Value::as_str) {
            Some("resource_link") => {
                if let Some(uri) = object.get("uri").and_then(Value::as_str) {
                    insert_uri(uri, paths);
                }
            }
            Some("resource") => {
                if let Some(uri) = object
                    .get("resource")
                    .and_then(|resource| resource.get("uri"))
                    .and_then(Value::as_str)
                {
                    insert_uri(uri, paths);
                }
            }
            Some("image") => {
                if let Some(path) = object.get("path").and_then(Value::as_str) {
                    insert_literal(path, paths);
                }
                for key in ["uri", "url"] {
                    if let Some(uri) = object.get(key).and_then(Value::as_str) {
                        insert_uri(uri, paths);
                    }
                }
            }
            Some("content") | None => {
                if let Some(content) = object.get("content") {
                    collect_output_resources(content, paths);
                }
            }
            _ => {}
        },
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_are_explicit_and_traversal_is_refused() {
        let paths = referenced_paths(
            "text",
            &serde_json::json!({"content":"plain /etc/passwd ![image](a.png) [file](/work/report.pdf)\n[[AION_FILES]]\n/work/upload.txt"}),
        );
        assert!(paths.contains("a.png"));
        assert!(paths.contains("/work/report.pdf"));
        assert!(paths.contains("/work/upload.txt"));
        let output = referenced_paths(
            "acp_tool_call",
            &serde_json::json!({"update":{"status":"completed","content":[{"type":"content","content":{"type":"resource","resource":{"uri":"file:///work/tool.png"}}}]}}),
        );
        assert!(output.contains("file:///work/tool.png"));
        assert!(!paths.contains("/etc/passwd"));
        assert!(resource_path("../secret", Some("/work")).is_none());
        assert!(resource_path("https://example.com/x", Some("/work")).is_none());
        assert_eq!(
            resource_path("a.png", Some("/work")),
            Some(PathBuf::from("/work/a.png"))
        );
    }

    #[test]
    fn markdown_destinations_are_decoded_once_and_reject_encoded_traversal() {
        let paths = referenced_paths(
            "text",
            &serde_json::json!({"content":"![image](</work/my image.png>) ![image](/work/plot(1).png) [encoded](/work/encoded%20space.png) [literal](/work/literal%2520name.png) [escape](%2e%2e/private.env)"}),
        );
        assert!(paths.contains("/work/my image.png"));
        assert!(paths.contains("/work/plot(1).png"));
        assert!(paths.contains("/work/encoded space.png"));
        assert!(paths.contains("/work/literal%20name.png"));
        assert!(!paths.contains("/work/literal name.png"));
        assert!(!paths.contains("%2e%2e/private.env"));
        assert!(!paths.contains("../private.env"));
    }

    #[test]
    fn input_aliases_and_markdown_code_do_not_become_resource_grants() {
        let input = serde_json::json!({"raw_input":{"path":"/work/private.env"},"rawInput":{"files":["/work/private.env"]},"input":{"file_path":"/work/private.env"},"args":{"content":"![image](/work/private.env)"},"path":"/work/private.env"});
        for kind in ["permission", "acp_permission", "tool_call", "acp_tool_call", "text"] {
            assert!(referenced_paths(kind, &input).is_empty(), "{kind}");
        }
        let text = serde_json::json!({"content":"`![image](/work/inline.env)`\n\n```markdown\n[file](/work/fenced.env)\n```\n"});
        assert!(referenced_paths("text", &text).is_empty());
    }
}
