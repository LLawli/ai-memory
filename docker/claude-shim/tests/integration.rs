//! End-to-end integration tests for the claude-shim translate layer
//! using a fake `claude` binary on disk.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use claude_shim::anthropic::{ContentBlock, Message, MessagesRequest, Tool, ToolChoice};
use claude_shim::translate;
use serde_json::{Value, json};
use tempfile::TempDir;

/// Write a fake `claude` binary that ignores stdin/args and prints
/// `payload` exactly once on stdout.
fn fake_claude(dir: &TempDir, payload: &str) -> PathBuf {
    let path = dir.path().join("claude");
    let script = format!("#!/bin/sh\ncat >/dev/null\ncat <<'JSON_EOF'\n{payload}\nJSON_EOF\n");
    fs::write(&path, script).unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
    path
}

fn request_text(user: &str) -> MessagesRequest {
    MessagesRequest {
        model: "claude-opus-4-7".into(),
        max_tokens: 64,
        system: Some("You are a test assistant.".into()),
        messages: vec![Message {
            role: "user".into(),
            content: user.into(),
        }],
        temperature: None,
        tools: None,
        tool_choice: None,
    }
}

fn request_structured(schema: Value) -> MessagesRequest {
    MessagesRequest {
        model: "claude-opus-4-7".into(),
        max_tokens: 64,
        system: None,
        messages: vec![Message {
            role: "user".into(),
            content: "extract".into(),
        }],
        temperature: None,
        tools: Some(vec![Tool {
            name: "result".into(),
            description: None,
            input_schema: schema,
        }]),
        tool_choice: Some(ToolChoice::Tool {
            name: "result".into(),
        }),
    }
}

#[tokio::test]
async fn text_mode_returns_text_content_block() {
    let dir = tempfile::tempdir().unwrap();
    let envelope = json!({
        "type": "result",
        "subtype": "success",
        "result": "pong",
        "usage": {"input_tokens": 3, "output_tokens": 1},
        "is_error": false,
    })
    .to_string();
    let fake = fake_claude(&dir, &envelope);

    let req = request_text("ping");
    let prepared =
        translate::prepare(&req, fake.into_os_string(), Duration::from_secs(10)).unwrap();
    let response = translate::execute(prepared).await.expect("execute ok");

    assert_eq!(response.kind, "message");
    assert_eq!(response.role, "assistant");
    assert_eq!(response.model, "claude-opus-4-7");
    assert_eq!(response.stop_reason, "end_turn");
    assert_eq!(response.usage.input_tokens, 3);
    assert_eq!(response.usage.output_tokens, 1);
    assert_eq!(response.content.len(), 1);
    match &response.content[0] {
        ContentBlock::Text { text } => assert_eq!(text, "pong"),
        other => panic!("expected text block, got {other:?}"),
    }
}

#[tokio::test]
async fn structured_mode_returns_tool_use_block() {
    let dir = tempfile::tempdir().unwrap();
    let result_text = "{\"answer\":\"42\"}";
    let envelope = json!({
        "type": "result",
        "subtype": "success",
        "result": result_text,
        "usage": {"input_tokens": 10, "output_tokens": 5},
        "is_error": false,
    })
    .to_string();
    let fake = fake_claude(&dir, &envelope);

    let schema = json!({
        "type": "object",
        "properties": {"answer": {"type": "string"}},
        "required": ["answer"]
    });
    let req = request_structured(schema);
    let prepared =
        translate::prepare(&req, fake.into_os_string(), Duration::from_secs(10)).unwrap();
    let response = translate::execute(prepared).await.expect("execute ok");

    assert_eq!(response.stop_reason, "tool_use");
    match &response.content[0] {
        ContentBlock::ToolUse { name, input, id } => {
            assert_eq!(name, "result");
            assert!(id.starts_with("toolu_shim_"));
            assert_eq!(input["answer"], "42");
        }
        other => panic!("expected tool_use block, got {other:?}"),
    }
}

#[tokio::test]
async fn structured_mode_strips_markdown_fences_from_result() {
    let dir = tempfile::tempdir().unwrap();
    let result_text = "```json\n{\"answer\":\"x\"}\n```";
    let envelope = json!({
        "type": "result",
        "subtype": "success",
        "result": result_text,
        "usage": {"input_tokens": 1, "output_tokens": 1},
        "is_error": false,
    })
    .to_string();
    let fake = fake_claude(&dir, &envelope);

    let schema = json!({
        "type": "object",
        "properties": {"answer": {"type": "string"}},
        "required": ["answer"]
    });
    let req = request_structured(schema);
    let prepared =
        translate::prepare(&req, fake.into_os_string(), Duration::from_secs(10)).unwrap();
    let response = translate::execute(prepared).await.expect("execute ok");

    match &response.content[0] {
        ContentBlock::ToolUse { input, .. } => assert_eq!(input["answer"], "x"),
        other => panic!("expected tool_use block, got {other:?}"),
    }
}

#[tokio::test]
async fn claude_reported_error_surfaces_as_api_error() {
    let dir = tempfile::tempdir().unwrap();
    let envelope = json!({
        "type": "result",
        "subtype": "error_during_execution",
        "result": "oauth token expired",
        "is_error": true,
    })
    .to_string();
    let fake = fake_claude(&dir, &envelope);

    let req = request_text("ping");
    let prepared =
        translate::prepare(&req, fake.into_os_string(), Duration::from_secs(10)).unwrap();
    let err = translate::execute(prepared).await.unwrap_err();
    assert!(err.error.message.contains("oauth token expired"));
}
