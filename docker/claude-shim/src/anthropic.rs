//! Anthropic Messages API request/response shapes.
//!
//! Only the subset the ai-memory `AnthropicProvider` actually emits is
//! modelled. Unknown fields are accepted on input (via `#[serde(default)]`
//! where useful) and never echoed back. The shim must produce a response
//! whose deserialisation succeeds against `crates/ai-memory-llm/src/anthropic.rs`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct MessagesRequest {
    pub model: String,
    // `claude -p` controls these internally; we accept them on the wire
    // for Anthropic-compat but cannot forward them as CLI flags.
    #[allow(dead_code)]
    pub max_tokens: u32,
    #[serde(default)]
    pub system: Option<String>,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    #[allow(dead_code)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub tools: Option<Vec<Tool>>,
    #[serde(default)]
    pub tool_choice: Option<ToolChoice>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Tool {
    pub name: String,
    // Accepted on the wire (Anthropic-compat) but unused — the schema
    // instruction we inject already carries enough context for claude.
    #[serde(default)]
    #[allow(dead_code)]
    pub description: Option<String>,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    Any,
    Tool { name: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct MessagesResponse {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub role: &'static str,
    pub model: String,
    pub content: Vec<ContentBlock>,
    pub stop_reason: &'static str,
    pub stop_sequence: Option<String>,
    pub usage: Usage,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiErrorBody {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub error: ApiErrorPayload,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiErrorPayload {
    #[serde(rename = "type")]
    pub kind: String,
    pub message: String,
}

impl ApiErrorBody {
    pub fn new(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind: "error",
            error: ApiErrorPayload {
                kind: kind.into(),
                message: message.into(),
            },
        }
    }
}
