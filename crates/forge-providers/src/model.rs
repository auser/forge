use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use forge_config::Config;
use forge_core::{
    CompletionRequest, CompletionResponse, ForgeError, ModelCapabilities, ModelProvider, Usage,
};
use serde::Serialize;

use crate::scripted::ScriptedMockModel;

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
        reject_tools_without_capability(&request, self.capabilities)?;
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
            tool_calls: Vec::new(),
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

/// Enforce the capability contract: providers without `tools` must reject
/// tool-bearing requests with a typed error.
pub(crate) fn reject_tools_without_capability(
    request: &CompletionRequest,
    capabilities: ModelCapabilities,
) -> Result<(), ForgeError> {
    if !capabilities.tools && !request.tools.is_empty() {
        return Err(ForgeError::provider(format!(
            "provider does not support tools (capabilities.tools = false), \
             but the request carries {} tool definition(s)",
            request.tools.len()
        )));
    }
    Ok(())
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ChatTool<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<ChatToolCall<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
}

/// OpenAI wire shape: `{"type":"function","function":{...}}`.
#[derive(Serialize)]
struct ChatTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: ChatToolFunction<'a>,
}

#[derive(Serialize)]
struct ChatToolFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
}

/// OpenAI wire shape for assistant tool calls; `arguments` is a JSON
/// *string* on the wire.
#[derive(Serialize)]
struct ChatToolCall<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    function: ChatToolCallFunction<'a>,
}

#[derive(Serialize)]
struct ChatToolCallFunction<'a> {
    name: &'a str,
    arguments: String,
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
        reject_tools_without_capability(&request, self.capabilities)?;
        let body = ChatRequest {
            model: &self.model,
            messages: request
                .messages
                .iter()
                .map(|m| ChatMessage {
                    role: role_name(m.role),
                    content: &m.content,
                    tool_calls: m
                        .tool_calls
                        .iter()
                        .map(|c| ChatToolCall {
                            id: &c.id,
                            kind: "function",
                            function: ChatToolCallFunction {
                                name: &c.name,
                                arguments: c.arguments.to_string(),
                            },
                        })
                        .collect(),
                    tool_call_id: m.tool_call_id.as_deref(),
                })
                .collect(),
            tools: request
                .tools
                .iter()
                .map(|t| ChatTool {
                    kind: "function",
                    function: ChatToolFunction {
                        name: &t.name,
                        description: &t.description,
                        parameters: &t.parameters,
                    },
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

        let message = body.pointer("/choices/0/message").ok_or_else(|| {
            ForgeError::provider(format!(
                "model response from {url} missing choices[0].message"
            ))
        })?;
        let content = message
            .get("content")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();

        // Parse OpenAI tool_calls: function.arguments arrives as a JSON
        // string; tolerate invalid JSON by keeping it as a string value.
        let tool_calls = message
            .get("tool_calls")
            .and_then(serde_json::Value::as_array)
            .map(|calls| {
                calls
                    .iter()
                    .filter_map(|call| {
                        let id = call.get("id")?.as_str()?;
                        let name = call.pointer("/function/name")?.as_str()?;
                        let raw = call
                            .pointer("/function/arguments")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("{}");
                        let arguments = serde_json::from_str(raw)
                            .unwrap_or_else(|_| serde_json::Value::String(raw.to_string()));
                        Some(forge_core::ToolCall::new(id, name, arguments))
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(CompletionResponse {
            model: self.model.clone(),
            content,
            tool_calls,
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
/// selects the offline mock; `scripted-mock` loads a scripted mock from the
/// `mock_script` JSON file (resolved against `project_root` when relative);
/// anything else produces an OpenAI-compatible client with tools explicitly
/// enabled (the OpenAI tools schema is supported by oMLX-class servers).
/// When no `model_base_url` is configured, the client points at a
/// guaranteed-unroutable loopback address so construction succeeds and the
/// failure surfaces as a typed provider error at request time — after
/// routing decisions have been recorded.
pub fn model_from_config(
    config: &Config,
    project_root: &std::path::Path,
) -> Result<Arc<dyn ModelProvider>, ForgeError> {
    match config.model.as_str() {
        "mock" | "mock-local" => Ok(Arc::new(MockModel::new())),
        "scripted-mock" => {
            let script = config.mock_script.as_deref().ok_or_else(|| {
                ForgeError::config(
                    "model = \"scripted-mock\" requires mock_script (path to a JSON script)",
                )
            })?;
            let path = std::path::PathBuf::from(script);
            let path = if path.is_absolute() {
                path
            } else {
                project_root.join(path)
            };
            Ok(Arc::new(ScriptedMockModel::from_path(&path)?))
        }
        name => {
            // A `[models.<name>]` entry resolves the endpoint and can
            // override capabilities; unset fields inherit the global
            // model_base_url/model_key_env.
            let entry = config.models.get(name);
            let base_url = entry
                .and_then(|e| e.base_url.clone())
                .or_else(|| config.model_base_url.clone())
                .unwrap_or_else(|| {
                    tracing::warn!(
                        model = name,
                        "no model_base_url configured; requests will fail"
                    );
                    "http://127.0.0.1:9".to_string()
                });
            let key_env = entry
                .and_then(|e| e.key_env.clone())
                .or_else(|| config.model_key_env.clone());
            let mut capabilities = ModelCapabilities {
                streaming: true,
                tools: true,
                structured_output: false,
                vision: false,
                max_context: 32_768,
            };
            if let Some(entry) = entry {
                // Explicit overrides win over the OpenAI-compatible defaults.
                if let Some(v) = entry.tools {
                    capabilities.tools = v;
                }
                if let Some(v) = entry.streaming {
                    capabilities.streaming = v;
                }
                if let Some(v) = entry.structured_output {
                    capabilities.structured_output = v;
                }
                if let Some(v) = entry.vision {
                    capabilities.vision = v;
                }
                if let Some(v) = entry.max_context {
                    capabilities.max_context = v;
                }
            }
            Ok(Arc::new(OpenAiCompatibleModel::new(
                base_url,
                name,
                key_env,
                capabilities,
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
        let model = model_from_config(&config, std::path::Path::new(".")).expect("mock builds");
        assert_eq!(model.name(), "mock-local");
    }

    #[tokio::test]
    async fn openai_compatible_sends_tools_and_parses_tool_calls() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "write_file",
                                "arguments": "{\"path\": \"src/main.rs\", \"content\": \"fn main() {}\"}"
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            })))
            .mount(&server)
            .await;

        let model = OpenAiCompatibleModel::new(
            server.uri(),
            "tool-model",
            None,
            ModelCapabilities {
                tools: true,
                ..ModelCapabilities::default()
            },
            Duration::from_secs(5),
        )
        .expect("construct");

        let request = CompletionRequest::new("tool-model", vec![Message::user("write the file")])
            .with_tools(vec![forge_core::ToolDefinition::new(
                "write_file",
                "Write a file",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "content": {"type": "string"}
                    },
                    "required": ["path", "content"]
                }),
            )]);
        let response = model.complete(request).await.expect("completes");

        assert_eq!(response.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "call_1");
        assert_eq!(response.tool_calls[0].name, "write_file");
        assert_eq!(response.tool_calls[0].arguments["path"], "src/main.rs");

        // Verify the request body used the OpenAI tools wire shape.
        let requests = server.received_requests().await.expect("requests");
        let body: serde_json::Value =
            serde_json::from_slice(&requests[0].body).expect("request json");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "write_file");
        assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
    }

    #[tokio::test]
    async fn openai_compatible_without_tools_capability_rejects_tool_requests() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0) // must never be called
            .mount(&server)
            .await;

        // Bare-URL default: tools capability OFF.
        let model = OpenAiCompatibleModel::new(
            server.uri(),
            "plain-model",
            None,
            ModelCapabilities::default(),
            Duration::from_secs(5),
        )
        .expect("construct");
        assert!(!model.capabilities().tools);

        let request =
            CompletionRequest::new("plain-model", vec![Message::user("x")]).with_tools(vec![
                forge_core::ToolDefinition::new(
                    "write_file",
                    "write",
                    serde_json::json!({"type": "object"}),
                ),
            ]);
        let err = model.complete(request).await.expect_err("must reject");
        assert!(matches!(err, ForgeError::Provider(_)));
    }

    #[tokio::test]
    async fn scripted_mock_loads_from_config_via_model_from_config() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("script.json"),
            r#"[{"text": "scripted hello"}]"#,
        )
        .expect("write script");
        let config = Config {
            model: "scripted-mock".to_string(),
            mock_script: Some("script.json".to_string()),
            ..Config::default()
        };
        let model = model_from_config(&config, tmp.path()).expect("builds");
        assert_eq!(model.name(), "scripted-mock");
        assert!(model.capabilities().tools);
        let response = model
            .complete(CompletionRequest::new(
                "scripted-mock",
                vec![Message::user("hi")],
            ))
            .await
            .expect("completes");
        assert_eq!(response.content, "scripted hello");

        // Missing script key is a typed config error.
        let config = Config {
            model: "scripted-mock".to_string(),
            ..Config::default()
        };
        match model_from_config(&config, tmp.path()) {
            Err(ForgeError::Config(_)) => {}
            other => panic!("expected config error, got {:?}", other.map(|_| ())),
        }
    }

    #[tokio::test]
    async fn model_without_base_url_fails_at_request_time() {
        // Construction succeeds (so routing decisions are still recorded);
        // the request fails fast against the unroutable placeholder URL.
        let config = Config {
            model: "gpt-ish".to_string(),
            ..Config::default()
        };
        let model = model_from_config(&config, std::path::Path::new(".")).expect("constructs");
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
