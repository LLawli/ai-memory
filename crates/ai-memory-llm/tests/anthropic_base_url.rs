//! Confirms that `ProviderConfig.base_url` is wired into
//! `AnthropicProvider` by `build_provider`, so the Anthropic
//! client can be redirected to a local shim (e.g. claude-shim).

use ai_memory_llm::{
    ChatMessage, ChatRequest, ProviderChoice, ProviderConfig, Role, build_provider,
};
use secrecy::SecretString;
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn build_provider_honours_anthropic_base_url() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "test-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_test",
            "type": "message",
            "role": "assistant",
            "model": "claude-opus-4-7",
            "content": [{"type": "text", "text": "pong"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5, "output_tokens": 1}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider = build_provider(ProviderConfig {
        provider: ProviderChoice::Anthropic,
        model: "claude-opus-4-7".to_string(),
        api_key: Some(SecretString::new("test-key".into())),
        base_url: Some(server.uri()),
    })
    .expect("build_provider");

    let response = provider
        .complete(ChatRequest {
            system: None,
            messages: vec![ChatMessage {
                role: Role::User,
                content: "ping".to_string(),
            }],
            max_tokens: 16,
            temperature: None,
        })
        .await
        .expect("complete");

    assert_eq!(response.text, "pong");
    assert_eq!(response.model, "claude-opus-4-7");
    let usage = response.usage.expect("usage present");
    assert_eq!(usage.input_tokens, 5);
    assert_eq!(usage.output_tokens, 1);
}
