use std::time::Duration;

use forge_core::{
    CompletionRequest, ForgeError, Message, ModelCapabilities, ModelProvider, ProviderFailureKind,
    ToolDefinition,
};
use forge_providers::{
    AnthropicModel, CodexModel, CredentialSource, EgressPolicy, OpenAiCompatibleModel,
    ResolvedCredential,
};
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

fn capabilities() -> ModelCapabilities {
    ModelCapabilities {
        streaming: true,
        tools: true,
        max_context: 200_000,
        ..ModelCapabilities::default()
    }
}

fn request(model: &str) -> CompletionRequest {
    CompletionRequest::new(model, vec![Message::user("contract probe")])
}

fn chat_response(model: &str, text: &str) -> serde_json::Value {
    json!({
        "model": model,
        "choices": [{
            "message": {"role": "assistant", "content": text},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 2, "completion_tokens": 1, "total_tokens": 3}
    })
}

fn openai_model(base_url: String, capabilities: ModelCapabilities) -> OpenAiCompatibleModel {
    OpenAiCompatibleModel::new(
        base_url,
        "contract-model",
        None,
        capabilities,
        Duration::from_millis(500),
        EgressPolicy::Unrestricted,
    )
    .expect("OpenAI-compatible provider")
}

#[tokio::test]
async fn claude_subscription_contract_is_anthropic_messages_with_oauth_headers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("authorization", "Bearer claude-oauth"))
        .and(header("anthropic-version", "2023-06-01"))
        .and(header("anthropic-beta", "oauth-2025-04-20"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "text", "text": "claude-ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 2, "output_tokens": 1}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let model = AnthropicModel::new(
        Some(server.uri()),
        "claude-sonnet",
        ResolvedCredential::oauth_token("claude-oauth", CredentialSource::ClaudeCodeCredentials),
        capabilities(),
        None,
        Duration::from_secs(2),
        EgressPolicy::Unrestricted,
    )
    .expect("claude provider");

    let response = model
        .complete(request("claude-sonnet"))
        .await
        .expect("claude contract");
    assert_eq!(response.content, "claude-ok");
    assert_eq!(response.usage.map(|usage| usage.total_tokens), Some(3));
}

#[tokio::test]
async fn codex_subscription_contract_is_responses_sse_with_account_scope() {
    let server = MockServer::start().await;
    let completed = json!({
        "type": "response.completed",
        "response": {
            "model": "gpt-5.6-sol",
            "status": "completed",
            "output": [{"type": "message", "content": [
                {"type": "output_text", "text": "codex-ok"}
            ]}],
            "usage": {"input_tokens": 2, "output_tokens": 1, "total_tokens": 3}
        }
    });
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(header("authorization", "Bearer codex-oauth"))
        .and(header("chatgpt-account-id", "account-1"))
        .and(header("openai-beta", "responses=v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(format!("data: {completed}\n\ndata: [DONE]\n\n")),
        )
        .expect(1)
        .mount(&server)
        .await;
    let model = CodexModel::new(
        Some(server.uri()),
        "gpt-5.6-sol",
        "codex-oauth",
        "account-1",
        Duration::from_secs(2),
        EgressPolicy::Unrestricted,
    )
    .expect("codex provider");

    let response = model
        .complete(request("gpt-5.6-sol"))
        .await
        .expect("codex contract");
    assert_eq!(response.content, "codex-ok");
    assert_eq!(response.usage.map(|usage| usage.total_tokens), Some(3));
}

#[tokio::test]
async fn kimi_code_contract_is_openai_chat_under_coding_v1_with_oauth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/coding/v1/chat/completions"))
        .and(header("authorization", "Bearer kimi-oauth"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_response("k3", "kimi-ok")))
        .expect(1)
        .mount(&server)
        .await;
    let model = OpenAiCompatibleModel::new(
        format!("{}/coding/v1", server.uri()),
        "k3",
        Some(ResolvedCredential::oauth_token(
            "kimi-oauth",
            CredentialSource::KimiCodeCredentials,
        )),
        capabilities(),
        Duration::from_secs(2),
        EgressPolicy::Unrestricted,
    )
    .expect("kimi provider");

    let response = model.complete(request("k3")).await.expect("kimi contract");
    assert_eq!(response.content, "kimi-ok");
}

#[tokio::test]
async fn local_contract_is_openai_chat_without_an_authorization_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(chat_response("qwen3-coder", "local-ok")),
        )
        .expect(1)
        .mount(&server)
        .await;
    let model = OpenAiCompatibleModel::new(
        format!("{}/v1", server.uri()),
        "qwen3-coder",
        None,
        capabilities(),
        Duration::from_secs(2),
        EgressPolicy::Unrestricted,
    )
    .expect("local provider");

    let response = model
        .complete(request("qwen3-coder"))
        .await
        .expect("local contract");
    assert_eq!(response.content, "local-ok");
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].headers.contains_key("authorization"));
}

#[tokio::test]
async fn failure_contract_distinguishes_auth_endpoint_capability_quota_and_response_shape() {
    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&auth)
        .await;
    let error = openai_model(auth.uri(), capabilities())
        .complete(request("contract-model"))
        .await
        .expect_err("authentication refusal");
    assert_eq!(
        error.provider_failure_kind(),
        Some(ProviderFailureKind::Authentication)
    );

    let error = openai_model("http://127.0.0.1:9/v1".into(), capabilities())
        .complete(request("contract-model"))
        .await
        .expect_err("unreachable endpoint");
    assert_eq!(
        error.provider_failure_kind(),
        Some(ProviderFailureKind::Endpoint)
    );

    let no_tools = ModelCapabilities {
        tools: false,
        ..capabilities()
    };
    let error = openai_model("http://127.0.0.1:9/v1".into(), no_tools)
        .complete(
            request("contract-model").with_tools(vec![ToolDefinition::new(
                "read_file",
                "read one file",
                json!({"type": "object"}),
            )]),
        )
        .await
        .expect_err("unsupported capability");
    assert_eq!(
        error.provider_failure_kind(),
        Some(ProviderFailureKind::Capability)
    );

    let quota = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "17"))
        .mount(&quota)
        .await;
    let error = openai_model(quota.uri(), capabilities())
        .complete(request("contract-model"))
        .await
        .expect_err("quota refusal");
    assert!(matches!(
        error,
        ForgeError::ProviderRateLimited {
            retry_after_seconds: Some(17),
            ..
        }
    ));

    let malformed = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"unexpected": true})))
        .mount(&malformed)
        .await;
    let error = openai_model(malformed.uri(), capabilities())
        .complete(request("contract-model"))
        .await
        .expect_err("wrong response shape");
    assert_eq!(
        error.provider_failure_kind(),
        Some(ProviderFailureKind::ResponseShape)
    );
}

#[tokio::test]
async fn claude_and_codex_malformed_successes_are_response_shape_failures() {
    let claude = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"unexpected": true})))
        .mount(&claude)
        .await;
    let claude_model = AnthropicModel::new(
        Some(claude.uri()),
        "claude-sonnet",
        ResolvedCredential::oauth_token("claude-oauth", CredentialSource::ClaudeCodeCredentials),
        capabilities(),
        None,
        Duration::from_secs(2),
        EgressPolicy::Unrestricted,
    )
    .expect("claude provider");
    let error = claude_model
        .complete(request("claude-sonnet"))
        .await
        .expect_err("wrong Claude response shape");
    assert_eq!(
        error.provider_failure_kind(),
        Some(ProviderFailureKind::ResponseShape)
    );

    let codex = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"unexpected": true})))
        .mount(&codex)
        .await;
    let codex_model = CodexModel::new(
        Some(codex.uri()),
        "gpt-5.6-sol",
        "codex-oauth",
        "account-1",
        Duration::from_secs(2),
        EgressPolicy::Unrestricted,
    )
    .expect("codex provider");
    let error = codex_model
        .complete(request("gpt-5.6-sol"))
        .await
        .expect_err("wrong Codex response shape");
    assert_eq!(
        error.provider_failure_kind(),
        Some(ProviderFailureKind::ResponseShape)
    );
}
