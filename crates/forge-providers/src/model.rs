use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use forge_config::Config;
use forge_core::{
    CompletionRequest, CompletionResponse, ForgeError, ModelCapabilities, ModelProvider, Usage,
};
use serde::Serialize;

/// Deterministic offline model. Echoes the last user message and records
/// every request for assertions. Capabilities are configurable so tests
/// can build restricted variants.
pub struct MockModel {
    name: String,
    capabilities: ModelCapabilities,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl Default for MockModel {
    fn default() -> Self {
        Self::new()
    }
}

impl MockModel {
    pub fn new() -> Self {
        Self {
            name: "mock-local".to_string(),
            capabilities: ModelCapabilities {
                streaming: true,
                tools: true,
                structured_output: true,
                vision: false,
                max_context: 32_768,
            },
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn with_capabilities(mut self, capabilities: ModelCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    pub fn recorded(&self) -> Vec<CompletionRequest> {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

#[async_trait]
impl ModelProvider for MockModel {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> ModelCapabilities {
        self.capabilities
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, ForgeError> {
        let prompt = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == forge_core::Role::User)
            .map(|m| m.content.clone())
            .unwrap_or_default();
        // Echo a snippet of any system context (e.g. activated skill
        // instructions) so context plumbing is observable in tests.
        let system: String = request
            .messages
            .iter()
            .filter(|m| m.role == forge_core::Role::System)
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(request.clone());
        let content = if system.is_empty() {
            format!("mock response to: {prompt}")
        } else {
            let snippet: String = system.chars().take(120).collect();
            format!("mock response to: {prompt} (system context: {snippet})")
        };
        Ok(CompletionResponse {
            model: self.name.clone(),
            usage: Some(Usage {
                prompt_tokens: prompt.len() as u32,
                completion_tokens: content.len() as u32,
                total_tokens: (prompt.len() + content.len()) as u32,
            }),
            finish_reason: Some("stop".to_string()),
            content,
        })
    }
}

/// OpenAI-compatible chat-completions client (works with oMLX and other
/// compatible servers). The API key is read from `api_key_env` at request
/// time and is never logged.
pub struct OpenAiCompatibleModel {
    client: reqwest::Client,
    base_url: String,
    model: String,
    api_key_env: Option<String>,
    capabilities: ModelCapabilities,
}

impl OpenAiCompatibleModel {
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key_env: Option<String>,
        capabilities: ModelCapabilities,
        timeout: Duration,
    ) -> Result<Self, ForgeError> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| ForgeError::provider(format!("building HTTP client: {e}")))?;
        Ok(Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            model: model.into(),
            api_key_env,
            capabilities,
        })
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

fn role_name(role: forge_core::Role) -> &'static str {
    match role {
        forge_core::Role::System => "system",
        forge_core::Role::User => "user",
        forge_core::Role::Assistant => "assistant",
        forge_core::Role::Tool => "tool",
    }
}

#[async_trait]
impl ModelProvider for OpenAiCompatibleModel {
    fn name(&self) -> &str {
        &self.model
    }

    fn capabilities(&self) -> ModelCapabilities {
        self.capabilities
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, ForgeError> {
        let body = ChatRequest {
            model: &self.model,
            messages: request
                .messages
                .iter()
                .map(|m| ChatMessage {
                    role: role_name(m.role),
                    content: &m.content,
                })
                .collect(),
            max_tokens: request.max_tokens,
            temperature: request.temperature,
        };
        let url = format!("{}/chat/completions", self.base_url);

        let mut http = self.client.post(&url).json(&body);
        if let Some(env_name) = &self.api_key_env
            && let Ok(key) = std::env::var(env_name)
            && !key.is_empty()
        {
            http = http.bearer_auth(key);
        }

        let response = http.send().await.map_err(|e| {
            if e.is_timeout() {
                ForgeError::provider(format!("model request to {url} timed out"))
            } else {
                ForgeError::provider(format!("model request to {url} failed: {e}"))
            }
        })?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(ForgeError::provider(format!(
                "model endpoint {url} returned {status}: {text}"
            )));
        }
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| ForgeError::provider(format!("reading model response from {url}: {e}")))?;

        let content = body
            .pointer("/choices/0/message/content")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ForgeError::provider(format!(
                    "model response from {url} missing choices[0].message.content"
                ))
            })?
            .to_string();

        Ok(CompletionResponse {
            model: self.model.clone(),
            content,
            finish_reason: body
                .pointer("/choices/0/finish_reason")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            usage: body.get("usage").and_then(|u| {
                Some(Usage {
                    prompt_tokens: u.get("prompt_tokens")?.as_u64()? as u32,
                    completion_tokens: u.get("completion_tokens")?.as_u64()? as u32,
                    total_tokens: u.get("total_tokens")?.as_u64()? as u32,
                })
            }),
        })
    }
}

/// Build the active model provider from configuration. `mock`/`mock-local`
/// selects the offline mock; anything else requires `model_base_url` and
/// produces an OpenAI-compatible client.
/// Build the active model provider from configuration. `mock`/`mock-local`
/// selects the offline mock; anything else produces an OpenAI-compatible
/// client. When no `model_base_url` is configured, the client points at a
/// guaranteed-unroutable loopback address so construction succeeds and the
/// failure surfaces as a typed provider error at request time — after
/// routing decisions have been recorded.
pub fn model_from_config(config: &Config) -> Result<Arc<dyn ModelProvider>, ForgeError> {
    match config.model.as_str() {
        "mock" | "mock-local" => Ok(Arc::new(MockModel::new())),
        name => {
            let base_url = config.model_base_url.clone().unwrap_or_else(|| {
                tracing::warn!(
                    model = name,
                    "no model_base_url configured; requests will fail"
                );
                "http://127.0.0.1:9".to_string()
            });
            Ok(Arc::new(OpenAiCompatibleModel::new(
                base_url,
                name,
                config.model_key_env.clone(),
                ModelCapabilities::default(),
                Duration::from_secs(120),
            )?))
        }
    }
}

#[cfg(test)]
mod tests {
    use forge_core::Message;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[tokio::test]
    async fn mock_model_echoes_prompt_and_records() {
        let model = MockModel::new();
        let response = model
            .complete(CompletionRequest::new(
                "mock-local",
                vec![Message::user("hello world")],
            ))
            .await
            .expect("mock completes");

        assert_eq!(response.content, "mock response to: hello world");
        assert_eq!(response.model, "mock-local");
        assert_eq!(response.finish_reason.as_deref(), Some("stop"));
        assert_eq!(model.recorded().len(), 1);
        assert!(model.capabilities().tools);
    }

    #[tokio::test]
    async fn mock_model_capabilities_are_configurable() {
        let model = MockModel::new().with_capabilities(ModelCapabilities {
            streaming: false,
            tools: false,
            structured_output: false,
            vision: false,
            max_context: 1_024,
        });
        let caps = model.capabilities();
        assert!(!caps.tools);
        assert!(!caps.streaming);
        assert_eq!(caps.max_context, 1_024);
    }

    #[tokio::test]
    async fn openai_compatible_maps_chat_completion_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "message": { "role": "assistant", "content": "hi there" },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5 }
            })))
            .mount(&server)
            .await;

        let model = OpenAiCompatibleModel::new(
            server.uri(),
            "test-model",
            None,
            ModelCapabilities::default(),
            Duration::from_secs(5),
        )
        .expect("construct");
        let response = model
            .complete(CompletionRequest::new(
                "test-model",
                vec![Message::user("ping")],
            ))
            .await
            .expect("completes");

        assert_eq!(response.content, "hi there");
        assert_eq!(response.model, "test-model");
        assert_eq!(response.usage.map(|u| u.total_tokens), Some(5));
    }

    #[tokio::test]
    async fn openai_compatible_sends_bearer_from_env_without_logging_it() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("authorization", "Bearer test-key-12345"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "content": "ok" } }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        unsafe { std::env::set_var("FORGE_PROVIDERS_TEST_KEY", "test-key-12345") };
        let model = OpenAiCompatibleModel::new(
            server.uri(),
            "m",
            Some("FORGE_PROVIDERS_TEST_KEY".to_string()),
            ModelCapabilities::default(),
            Duration::from_secs(5),
        )
        .expect("construct");
        let response = model
            .complete(CompletionRequest::new("m", vec![Message::user("x")]))
            .await
            .expect("completes");
        assert_eq!(response.content, "ok");
        unsafe { std::env::remove_var("FORGE_PROVIDERS_TEST_KEY") };
    }

    #[tokio::test]
    async fn openai_compatible_maps_http_errors_to_provider_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let model = OpenAiCompatibleModel::new(
            server.uri(),
            "m",
            None,
            ModelCapabilities::default(),
            Duration::from_secs(5),
        )
        .expect("construct");
        let err = model
            .complete(CompletionRequest::new("m", vec![Message::user("x")]))
            .await
            .expect_err("must fail");
        match err {
            ForgeError::Provider(msg) => {
                assert!(msg.contains("500"), "got: {msg}");
                assert!(!msg.contains("test-key"), "must never leak keys: {msg}");
            }
            other => panic!("expected provider error, got {other:?}"),
        }
    }

    #[test]
    fn model_from_config_defaults_to_offline_mock() {
        let config = Config::default();
        let model = model_from_config(&config).expect("mock builds");
        assert_eq!(model.name(), "mock-local");
    }

    #[tokio::test]
    async fn model_without_base_url_fails_at_request_time() {
        // Construction succeeds (so routing decisions are still recorded);
        // the request fails fast against the unroutable placeholder URL.
        let config = Config {
            model: "gpt-ish".to_string(),
            ..Config::default()
        };
        let model = model_from_config(&config).expect("constructs");
        assert_eq!(model.name(), "gpt-ish");
        let err = model
            .complete(CompletionRequest::new(
                "gpt-ish",
                vec![Message::user("ping")],
            ))
            .await
            .expect_err("unroutable endpoint must fail");
        assert!(matches!(err, ForgeError::Provider(_)));
    }
}
