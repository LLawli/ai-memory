//! Translation between Anthropic Messages requests/responses and
//! `claude -p` invocations.

use std::time::Duration;

use jsonschema::Validator;
use tracing::warn;
use uuid::Uuid;

use crate::anthropic::{
    ApiErrorBody, ContentBlock, MessagesRequest, MessagesResponse, Tool, ToolChoice, Usage,
};
use crate::claude::{self, ClaudeError, Envelope, Invocation};

const STRUCTURED_INSTRUCTION_HEADER: &str = "You MUST respond with ONLY a single JSON object that validates against this schema. No prose, no markdown fences, no explanation. Just the JSON object.";

/// Outcome of one structured-mode attempt before retry logic.
enum StructuredOutcome {
    Ok(serde_json::Value),
    Invalid(String),
}

/// Translate a `MessagesRequest` into the `claude -p` invocation +
/// optional structured-mode schema.
pub struct PreparedCall<'a> {
    pub invocation: Invocation,
    pub structured: Option<StructuredSpec<'a>>,
    pub model_echo: String,
}

/// Schema + tool name to use when the caller asked for tool_use output.
pub struct StructuredSpec<'a> {
    pub tool_name: String,
    pub schema: &'a serde_json::Value,
}

/// Decide whether the request is in text or structured mode and
/// assemble the `Invocation` for the first attempt.
pub fn prepare<'a>(
    request: &'a MessagesRequest,
    binary: std::ffi::OsString,
    timeout: Duration,
) -> Result<PreparedCall<'a>, ApiErrorBody> {
    let user_prompt = join_messages(&request.messages);
    let structured = detect_structured(request);

    let mut system_prompt = request.system.clone().unwrap_or_default();
    if let Some(spec) = &structured {
        append_schema_instruction(&mut system_prompt, spec.schema);
    }

    Ok(PreparedCall {
        invocation: Invocation {
            binary,
            model: request.model.clone(),
            system_prompt,
            user_prompt,
            timeout,
        },
        structured,
        model_echo: request.model.clone(),
    })
}

fn detect_structured(request: &MessagesRequest) -> Option<StructuredSpec<'_>> {
    let choice = match &request.tool_choice {
        Some(ToolChoice::Tool { name }) => name.clone(),
        _ => return None,
    };
    let tools = request.tools.as_ref()?;
    let tool = tools.iter().find(|t: &&Tool| t.name == choice)?;
    Some(StructuredSpec {
        tool_name: tool.name.clone(),
        schema: &tool.input_schema,
    })
}

fn join_messages(messages: &[crate::anthropic::Message]) -> String {
    // ai-memory call sites are all single-turn (one user message).
    // For multi-turn we fall back to a labelled transcript — good
    // enough for the rare prompts that include prior assistant turns.
    if messages.len() == 1 && messages[0].role == "user" {
        return messages[0].content.clone();
    }
    let mut out = String::new();
    for m in messages {
        let label = match m.role.as_str() {
            "user" => "User",
            "assistant" => "Assistant",
            other => other,
        };
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(label);
        out.push_str(": ");
        out.push_str(&m.content);
    }
    out
}

fn append_schema_instruction(system_prompt: &mut String, schema: &serde_json::Value) {
    if !system_prompt.is_empty() {
        system_prompt.push_str("\n\n");
    }
    system_prompt.push_str(STRUCTURED_INSTRUCTION_HEADER);
    system_prompt.push_str("\n\nSchema:\n");
    system_prompt.push_str(&serde_json::to_string_pretty(schema).unwrap_or_default());
}

/// Execute the prepared call, with a single retry on schema-validation
/// failure in structured mode.
pub async fn execute(prepared: PreparedCall<'_>) -> Result<MessagesResponse, ApiErrorBody> {
    let model_echo = prepared.model_echo.clone();
    let envelope = match claude::run(&prepared.invocation).await {
        Ok(env) => env,
        Err(err) => return Err(map_claude_error(&err)),
    };

    match prepared.structured.as_ref() {
        None => Ok(text_response(model_echo, envelope)),
        Some(spec) => match try_structured(&envelope.result, spec.schema) {
            StructuredOutcome::Ok(value) => Ok(structured_response(
                model_echo,
                envelope.usage.input_tokens,
                envelope.usage.output_tokens,
                spec.tool_name.clone(),
                value,
            )),
            StructuredOutcome::Invalid(reason) => {
                warn!(reason = %reason, "structured output invalid; retrying once");
                let retry_invocation = build_retry_invocation(&prepared.invocation, &reason);
                let retry_envelope = match claude::run(&retry_invocation).await {
                    Ok(env) => env,
                    Err(err) => return Err(map_claude_error(&err)),
                };
                match try_structured(&retry_envelope.result, spec.schema) {
                    StructuredOutcome::Ok(value) => Ok(structured_response(
                        model_echo,
                        envelope.usage.input_tokens + retry_envelope.usage.input_tokens,
                        envelope.usage.output_tokens + retry_envelope.usage.output_tokens,
                        spec.tool_name.clone(),
                        value,
                    )),
                    StructuredOutcome::Invalid(reason) => Err(ApiErrorBody::new(
                        "invalid_response_error",
                        format!(
                            "claude produced JSON that failed schema validation twice: {reason}"
                        ),
                    )),
                }
            }
        },
    }
}

fn build_retry_invocation(original: &Invocation, reason: &str) -> Invocation {
    let mut sys = original.system_prompt.clone();
    sys.push_str("\n\nYour previous response failed schema validation:\n");
    sys.push_str(reason);
    sys.push_str("\nOutput ONLY a valid JSON object matching the schema. No prose.");
    Invocation {
        binary: original.binary.clone(),
        model: original.model.clone(),
        system_prompt: sys,
        user_prompt: original.user_prompt.clone(),
        timeout: original.timeout,
    }
}

fn try_structured(raw: &str, schema: &serde_json::Value) -> StructuredOutcome {
    let stripped = strip_fences(raw);
    let value: serde_json::Value = match serde_json::from_str(stripped) {
        Ok(v) => v,
        Err(e) => return StructuredOutcome::Invalid(format!("JSON parse error: {e}")),
    };
    let validator = match Validator::new(schema) {
        Ok(v) => v,
        Err(e) => {
            // Caller schema is malformed — not the model's fault. Surface
            // as the parse outcome so retry will fail too and we return
            // a clear error.
            return StructuredOutcome::Invalid(format!("schema compile error: {e}"));
        }
    };
    if let Err(err) = validator.validate(&value) {
        let detail = format!("{err}");
        return StructuredOutcome::Invalid(format!("schema violation: {detail}"));
    }
    StructuredOutcome::Ok(value)
}

/// Strip a leading ```json / ``` fence and trailing ``` if present.
/// Defensive — the model is instructed not to emit them.
fn strip_fences(raw: &str) -> &str {
    let trimmed = raw.trim();
    let without_open = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(str::trim_start)
        .unwrap_or(trimmed);
    without_open
        .strip_suffix("```")
        .map(str::trim_end)
        .unwrap_or(without_open)
}

fn text_response(model: String, envelope: Envelope) -> MessagesResponse {
    MessagesResponse {
        id: format!("msg_shim_{}", Uuid::new_v4()),
        kind: "message",
        role: "assistant",
        model,
        content: vec![ContentBlock::Text {
            text: envelope.result,
        }],
        stop_reason: "end_turn",
        stop_sequence: None,
        usage: Usage {
            input_tokens: envelope.usage.input_tokens,
            output_tokens: envelope.usage.output_tokens,
        },
    }
}

fn structured_response(
    model: String,
    input_tokens: u32,
    output_tokens: u32,
    tool_name: String,
    input: serde_json::Value,
) -> MessagesResponse {
    MessagesResponse {
        id: format!("msg_shim_{}", Uuid::new_v4()),
        kind: "message",
        role: "assistant",
        model,
        content: vec![ContentBlock::ToolUse {
            id: format!("toolu_shim_{}", Uuid::new_v4()),
            name: tool_name,
            input,
        }],
        stop_reason: "tool_use",
        stop_sequence: None,
        usage: Usage {
            input_tokens,
            output_tokens,
        },
    }
}

fn map_claude_error(err: &ClaudeError) -> ApiErrorBody {
    match err {
        ClaudeError::Spawn(_) => {
            ApiErrorBody::new("api_error", format!("failed to spawn claude: {err}"))
        }
        ClaudeError::NonZero { .. } => ApiErrorBody::new("api_error", err.to_string()),
        ClaudeError::Timeout(_) => ApiErrorBody::new("overloaded_error", err.to_string()),
        ClaudeError::ParseEnvelope { .. } => ApiErrorBody::new("api_error", err.to_string()),
        ClaudeError::Reported { .. } => ApiErrorBody::new("api_error", err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::Message;
    use serde_json::json;

    fn req_text(system: Option<&str>, user: &str) -> MessagesRequest {
        MessagesRequest {
            model: "claude-opus-4-7".into(),
            max_tokens: 64,
            system: system.map(str::to_string),
            messages: vec![Message {
                role: "user".into(),
                content: user.into(),
            }],
            temperature: None,
            tools: None,
            tool_choice: None,
        }
    }

    fn req_structured(schema: serde_json::Value) -> MessagesRequest {
        MessagesRequest {
            model: "claude-opus-4-7".into(),
            max_tokens: 64,
            system: Some("You are a test assistant.".into()),
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

    #[test]
    fn prepare_text_uses_user_message_as_prompt() {
        let req = req_text(Some("sys"), "hello");
        let prepared = prepare(&req, "claude".into(), Duration::from_secs(1)).unwrap();
        assert_eq!(prepared.invocation.user_prompt, "hello");
        assert_eq!(prepared.invocation.system_prompt, "sys");
        assert!(prepared.structured.is_none());
    }

    #[test]
    fn prepare_structured_appends_schema_to_system_prompt() {
        let schema = json!({
            "type": "object",
            "properties": {"answer": {"type": "string"}},
            "required": ["answer"]
        });
        let req = req_structured(schema.clone());
        let prepared = prepare(&req, "claude".into(), Duration::from_secs(1)).unwrap();
        let spec = prepared.structured.expect("structured detected");
        assert_eq!(spec.tool_name, "result");
        assert!(
            prepared
                .invocation
                .system_prompt
                .contains(STRUCTURED_INSTRUCTION_HEADER)
        );
        assert!(prepared.invocation.system_prompt.contains("\"answer\""));
        assert!(
            prepared
                .invocation
                .system_prompt
                .starts_with("You are a test assistant.")
        );
    }

    #[test]
    fn prepare_ignores_tool_choice_auto() {
        let mut req = req_structured(json!({"type": "object"}));
        req.tool_choice = Some(ToolChoice::Auto);
        let prepared = prepare(&req, "claude".into(), Duration::from_secs(1)).unwrap();
        assert!(prepared.structured.is_none());
    }

    #[test]
    fn join_messages_single_user_returns_content() {
        let msgs = vec![Message {
            role: "user".into(),
            content: "hi".into(),
        }];
        assert_eq!(join_messages(&msgs), "hi");
    }

    #[test]
    fn join_messages_multi_turn_uses_labels() {
        let msgs = vec![
            Message {
                role: "user".into(),
                content: "ping".into(),
            },
            Message {
                role: "assistant".into(),
                content: "pong".into(),
            },
            Message {
                role: "user".into(),
                content: "again".into(),
            },
        ];
        let joined = join_messages(&msgs);
        assert!(joined.contains("User: ping"));
        assert!(joined.contains("Assistant: pong"));
        assert!(joined.contains("User: again"));
    }

    #[test]
    fn strip_fences_handles_json_block() {
        assert_eq!(strip_fences("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_fences("```\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_fences("{\"a\":1}"), "{\"a\":1}");
    }

    #[test]
    fn try_structured_accepts_valid_json() {
        let schema = json!({
            "type": "object",
            "properties": {"a": {"type": "string"}},
            "required": ["a"]
        });
        match try_structured("{\"a\":\"x\"}", &schema) {
            StructuredOutcome::Ok(v) => assert_eq!(v["a"], "x"),
            StructuredOutcome::Invalid(msg) => panic!("expected ok, got invalid: {msg}"),
        }
    }

    #[test]
    fn try_structured_rejects_missing_required_field() {
        let schema = json!({
            "type": "object",
            "properties": {"a": {"type": "string"}},
            "required": ["a"]
        });
        match try_structured("{}", &schema) {
            StructuredOutcome::Invalid(msg) => assert!(msg.contains("schema violation")),
            StructuredOutcome::Ok(_) => panic!("expected invalid"),
        }
    }

    #[test]
    fn try_structured_rejects_non_json() {
        let schema = json!({"type": "object"});
        match try_structured("not json", &schema) {
            StructuredOutcome::Invalid(msg) => assert!(msg.contains("JSON parse error")),
            StructuredOutcome::Ok(_) => panic!("expected invalid"),
        }
    }
}
