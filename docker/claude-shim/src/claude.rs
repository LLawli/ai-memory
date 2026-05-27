//! Wrapper around `claude -p` (Claude Code CLI in headless mode).
//!
//! Spawns the binary via `tokio::process::Command`, captures stdout
//! (the envelope is a single JSON object), and surfaces structured
//! errors when the binary fails or the envelope is malformed.

use std::ffi::OsString;
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::debug;

/// Single JSON object emitted by `claude -p --output-format json`.
///
/// Extra fields are ignored — we only need the result text, usage and
/// the error flag.
#[derive(Debug, Clone, Deserialize)]
pub struct Envelope {
    #[serde(default)]
    pub result: String,
    #[serde(default)]
    pub usage: EnvelopeUsage,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default)]
    pub subtype: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct EnvelopeUsage {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
}

#[derive(Debug, Error)]
pub enum ClaudeError {
    #[error("claude binary spawn failed: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("claude subprocess exited with status {status:?}; stderr: {stderr}")]
    NonZero { status: Option<i32>, stderr: String },
    #[error("claude subprocess timed out after {0:?}")]
    Timeout(Duration),
    #[error("failed to parse claude envelope: {source}; raw: {raw}")]
    ParseEnvelope {
        #[source]
        source: serde_json::Error,
        raw: String,
    },
    #[error("claude reported an error: {message}")]
    Reported { message: String },
}

/// Inputs to a single `claude -p` invocation.
#[derive(Debug, Clone)]
pub struct Invocation {
    pub binary: OsString,
    pub model: String,
    pub system_prompt: String,
    pub user_prompt: String,
    pub timeout: Duration,
}

/// Run a single `claude -p ...` call and return the parsed envelope.
///
/// The prompt is delivered on stdin (via `claude -p` with no positional
/// arg) so prompts of arbitrary size and content don't blow argv limits
/// or get mangled by shell quoting. The system prompt is passed via
/// `--system-prompt` (a flag, bounded in size by our schema instruction).
pub async fn run(invocation: &Invocation) -> Result<Envelope, ClaudeError> {
    let mut cmd = Command::new(&invocation.binary);
    cmd.arg("-p")
        .arg("--output-format")
        .arg("json")
        .arg("--model")
        .arg(&invocation.model)
        .arg("--system-prompt")
        .arg(&invocation.system_prompt)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    debug!(model = %invocation.model, "spawning claude");
    let mut child = cmd.spawn().map_err(ClaudeError::Spawn)?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(invocation.user_prompt.as_bytes())
            .await
            .map_err(ClaudeError::Spawn)?;
        stdin.shutdown().await.map_err(ClaudeError::Spawn)?;
    }

    let output = match timeout(invocation.timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(err)) => return Err(ClaudeError::Spawn(err)),
        Err(_) => return Err(ClaudeError::Timeout(invocation.timeout)),
    };

    if !output.status.success() {
        return Err(ClaudeError::NonZero {
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    let raw = String::from_utf8_lossy(&output.stdout).into_owned();
    let envelope: Envelope =
        serde_json::from_str(&raw).map_err(|source| ClaudeError::ParseEnvelope {
            source,
            raw: raw.chars().take(512).collect(),
        })?;

    if envelope.is_error {
        let message = if envelope.result.is_empty() {
            envelope
                .subtype
                .clone()
                .unwrap_or_else(|| "unknown error".to_string())
        } else {
            envelope.result.clone()
        };
        return Err(ClaudeError::Reported { message });
    }

    Ok(envelope)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_parses_success() {
        let raw = r#"{"type":"result","subtype":"success","result":"hello","usage":{"input_tokens":4,"output_tokens":1},"is_error":false}"#;
        let env: Envelope = serde_json::from_str(raw).unwrap();
        assert_eq!(env.result, "hello");
        assert!(!env.is_error);
        assert_eq!(env.usage.input_tokens, 4);
        assert_eq!(env.usage.output_tokens, 1);
    }

    #[test]
    fn envelope_ignores_unknown_fields() {
        let raw =
            r#"{"type":"result","result":"hi","cost":0.001,"duration_ms":42,"session_id":"abc"}"#;
        let env: Envelope = serde_json::from_str(raw).unwrap();
        assert_eq!(env.result, "hi");
    }

    #[test]
    fn envelope_defaults_when_fields_missing() {
        let raw = r#"{"type":"result"}"#;
        let env: Envelope = serde_json::from_str(raw).unwrap();
        assert_eq!(env.result, "");
        assert!(!env.is_error);
        assert_eq!(env.usage.input_tokens, 0);
    }
}
