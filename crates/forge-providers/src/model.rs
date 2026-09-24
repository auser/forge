use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use forge_config::Config;
use forge_core::{
    CompletionRequest, CompletionResponse, ForgeError, ModelCapabilities, ModelProvider, Usage,
};
use serde::Serialize;

use crate::scripted::ScriptedMockModel;

/// Env var that turns the mock's system-context echo back on.
const MOCK_VERBOSE_ENV: &str = "FORGE_MOCK_VERBOSE";

/// Deterministic offline model. Replies `mock response to: <prompt>` and
/// records every request for assertions. Capabilities are configurable so
/// tests can build restricted variants.
///
/// The reply is deliberately clean: `forge --model mock-local run ...` is
/// the zero-setup first impression, and echoing the assembled system
/// context (skill instructions, graph context) into it made that output
/// look like a leak. Set `FORGE_MOCK_VERBOSE=1` to append
/// `(system context: <first 120 chars>)` when you need context plumbing
/// visible in a reply; tests that own the provider should prefer
/// [`MockModel::recorded`], which shows the whole request, untruncated.
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
        // Opt-in only (see the type docs): echoing the assembled system
        // context turns the one zero-setup command into a wall of internals.
        let system: String = if mock_verbose() {
            request
                .messages
                .iter()
                .filter(|m| m.role == forge_core::Role::System)
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            String::new()
        };
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

/// Whether the mock should append its system-context snippet. Anything but
/// unset/empty/`0`/`false` counts as on.
fn mock_verbose() -> bool {
    match std::env::var(MOCK_VERBOSE_ENV) {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "" | "0" | "false"),
        Err(_) => false,
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

/// The config field that named a credential env var, so the "remove it"
/// half of a hint points at a line that actually exists in the user's file.
pub(crate) const FIELD_MODEL_KEY_ENV: &str = "model_key_env";
/// `[models.<name>] key_env` named it, not the top-level `model_key_env`.
pub(crate) const FIELD_ENTRY_KEY_ENV: &str = "the model entry's key_env";

/// The one-line fix for a credential problem, safe to print anywhere: it
/// names the env var the configuration points at, never a value.
///
/// Both halves of the real-world failure need saying, because either can
/// be the actual mistake: the config names `OMLX_API_KEY` and the shell
/// doesn't have it (export it), or the endpoint never wanted a key at all
/// and the setting is leftover (delete it). `config_field` is which line to
/// delete — telling someone to remove `model_key_env` when their key came
/// from a `[models]` entry sends them looking for a line that isn't there.
pub(crate) fn credential_hint(key_env: Option<&str>, config_field: &str) -> String {
    match key_env {
        Some(name) => format!(
            "hint: set {name} in your shell or .env, or remove {config_field} \
             if the endpoint needs no key"
        ),
        None => "hint: no model_key_env is configured; set model_key_env = \"<ENV_VAR>\" \
                 (and export that variable) if this endpoint requires a key"
            .to_string(),
    }
}

/// OpenAI-compatible chat-completions client (works with oMLX and other
/// compatible servers). Holds a resolved credential (never logged); when
/// none was found, requests go out unauthenticated with a one-time warn.
pub struct OpenAiCompatibleModel {
    client: reqwest::Client,
    base_url: String,
    model: String,
    credential: Option<crate::credentials::ResolvedCredential>,
    capabilities: ModelCapabilities,
    /// Name of the env var the config expects the key in (never a value),
    /// carried purely so credential warnings and 401s can name the fix.
    key_env: Option<String>,
    /// Which config field named `key_env` (see [`credential_hint`]).
    key_env_field: &'static str,
}

impl OpenAiCompatibleModel {
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        credential: Option<crate::credentials::ResolvedCredential>,
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
            credential,
            capabilities,
            key_env: None,
            key_env_field: FIELD_MODEL_KEY_ENV,
        })
    }

    /// Record the env var name the configuration expects the key in, and
    /// which config field named it, so credential warnings and 401 errors
    /// can spell out the fix. Names only — the value never travels here.
    pub fn with_key_env(mut self, key_env: Option<String>, config_field: &'static str) -> Self {
        self.key_env = key_env;
        self.key_env_field = config_field;
        self
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
        match &self.credential {
            Some(credential) => http = http.bearer_auth(&credential.secret),
            // No credential resolved: send unauthenticated but warn loudly
            // — this is the classic silent-401 cause. Sources only, never
            // values, plus the exact fix.
            None => tracing::warn!(
                "no credential resolved for model {}; requests will be sent \
                 without authentication (see `forge auth status`) — {}",
                self.model,
                credential_hint(self.key_env.as_deref(), self.key_env_field)
            ),
        }

        let response = http.send().await.map_err(|e| {
            if e.is_timeout() {
                ForgeError::provider(format!("model request to {url} timed out"))
            } else if e.is_connect() {
                ForgeError::provider(format!(
                    "cannot reach OpenAI-compatible server at {url} (connection refused); \
                     start your model server (e.g. oMLX) or set model = \"mock-local\" for offline use"
                ))
            } else {
                ForgeError::provider(format!("model request to {url} failed: {e}"))
            }
        })?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            // 401/403 from the model endpoint is the one failure whose fix
            // is always a credential, so say which env var to set right
            // here rather than making the user go find `forge auth status`.
            let suffix = if matches!(status.as_u16(), 401 | 403) {
                format!(
                    " — {}",
                    credential_hint(self.key_env.as_deref(), self.key_env_field)
                )
            } else {
                String::new()
            };
            return Err(ForgeError::provider(format!(
                "model endpoint {url} returned {status}: {text}{suffix}"
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
            // override capabilities; an explicitly changed global
            // model_base_url (different from the built-in default) wins
            // over the entry's URL so env/CLI/file overrides always work.
            const DEFAULT_MODEL_BASE_URL: &str = "http://127.0.0.1:8080/v1";
            let entry = config.models.get(name);
            let entry_url = entry.and_then(|e| e.base_url.clone());
            let base_url = match (entry_url, &config.model_base_url) {
                (Some(_), Some(global)) if global != DEFAULT_MODEL_BASE_URL => global.clone(),
                (Some(entry_url), _) => entry_url,
                (None, Some(global)) => global.clone(),
                (None, None) => {
                    tracing::warn!(
                        model = name,
                        "no model_base_url configured; requests will fail"
                    );
                    "http://127.0.0.1:9".to_string()
                }
            };
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
            // Provider family: explicit entry.provider wins, else infer
            // from the endpoint host.
            let hint = entry
                .and_then(|e| e.provider.clone())
                .or_else(|| infer_provider_hint(&base_url));
            // Which config line named the key env var, so credential hints
            // can tell the user what to delete without sending them after a
            // line that isn't in their file.
            let (key_env, key_env_field) = match entry.and_then(|e| e.key_env.clone()) {
                Some(name) => (Some(name), FIELD_ENTRY_KEY_ENV),
                None => (config.model_key_env.clone(), FIELD_MODEL_KEY_ENV),
            };

            if hint.as_deref() == Some("anthropic") {
                let credential =
                    crate::credentials::resolve_credential(key_env.as_deref(), Some("anthropic"))
                        .ok_or_else(|| {
                        ForgeError::provider(format!(
                            "no credential for anthropic model {name:?}; tried {key_env_display}, \
                         CLAUDE_CODE_OAUTH_TOKEN, ~/.claude/.credentials.json \
                         (see `forge auth status`) — {hint}, or run `claude login`",
                            key_env_display = key_env.as_deref().unwrap_or("ANTHROPIC_API_KEY"),
                            hint = credential_hint(key_env.as_deref(), key_env_field)
                        ))
                    })?;
                let base = entry
                    .and_then(|e| e.base_url.clone())
                    .unwrap_or_else(|| base_url.clone());
                return Ok(Arc::new(crate::anthropic::AnthropicModel::new(
                    Some(base),
                    name,
                    credential,
                    capabilities,
                    entry.and_then(|e| e.max_output_tokens),
                    Duration::from_secs(120),
                )?));
            }

            let credential =
                crate::credentials::resolve_credential(key_env.as_deref(), hint.as_deref());
            if credential.is_none() && key_env.is_some() {
                tracing::warn!(
                    "no credential found for model {name}; tried env vars and CLI \
                     credential stores (see `forge auth status`) — {}",
                    credential_hint(key_env.as_deref(), key_env_field)
                );
            }
            Ok(Arc::new(
                OpenAiCompatibleModel::new(
                    base_url,
                    name,
                    credential,
                    capabilities,
                    Duration::from_secs(120),
                )?
                .with_key_env(key_env, key_env_field),
            ))
        }
    }
}

/// Infer the provider family from an endpoint URL.
fn infer_provider_hint(base_url: &str) -> Option<String> {
    if base_url.contains("anthropic") {
        Some("anthropic".to_string())
    } else if base_url.contains("openai.com") {
        Some("openai".to_string())
    } else if base_url.contains("moonshot") {
        Some("moonshot".to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use forge_core::Message;
    use serial_test::serial;
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

    /// The zero-setup first impression must stay clean: no assembled
    /// system context (skill instructions, graph context) in the reply,
    /// even when the request carries plenty of it. The request itself is
    /// still fully inspectable via `recorded()`.
    #[tokio::test]
    #[serial]
    async fn mock_model_reply_omits_system_context_by_default() {
        // SAFETY: test-only env mutation, serialized against the other
        // FORGE_MOCK_VERBOSE test via #[serial].
        unsafe { std::env::remove_var(MOCK_VERBOSE_ENV) };
        let model = MockModel::new();
        let response = model
            .complete(CompletionRequest::new(
                "mock-local",
                vec![
                    Message::system("Active skill `find-skills` instructions: # Find Skills"),
                    Message::user("Explain this project"),
                ],
            ))
            .await
            .expect("mock completes");

        assert_eq!(response.content, "mock response to: Explain this project");
        // The context still reached the provider — it just isn't echoed.
        let recorded = model.recorded();
        assert!(
            recorded[0]
                .messages
                .iter()
                .any(|m| m.content.contains("find-skills")),
            "the system context must still reach the model: {:?}",
            recorded[0].messages
        );
    }

    #[tokio::test]
    #[serial]
    async fn mock_model_echoes_system_context_when_verbose_env_is_set() {
        // SAFETY: test-only env mutation, serialized via #[serial].
        unsafe { std::env::set_var(MOCK_VERBOSE_ENV, "1") };
        let model = MockModel::new();
        let response = model
            .complete(CompletionRequest::new(
                "mock-local",
                vec![
                    Message::system("skill instructions here"),
                    Message::user("hi"),
                ],
            ))
            .await
            .expect("mock completes");
        unsafe { std::env::remove_var(MOCK_VERBOSE_ENV) };

        assert!(
            response
                .content
                .contains("system context: skill instructions here"),
            "content: {}",
            response.content
        );
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
            Some(crate::credentials::ResolvedCredential::api_key(
                "test-key-12345",
                crate::credentials::CredentialSource::EnvVar(
                    "FORGE_PROVIDERS_TEST_KEY".to_string(),
                ),
            )),
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

    /// The real-world failure: a config naming `OMLX_API_KEY`, the var
    /// unset, and a local server that wants a key. The 401 has to carry
    /// both fixes, and never the key itself.
    #[tokio::test]
    async fn unauthorized_error_names_the_key_env_var_to_set() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_string(r#"{"error":{"message":"API key required"}}"#),
            )
            .mount(&server)
            .await;

        let model = OpenAiCompatibleModel::new(
            server.uri(),
            "Qwen3-Coder-Next-4bit",
            None,
            ModelCapabilities::default(),
            Duration::from_secs(5),
        )
        .expect("construct")
        .with_key_env(Some("OMLX_API_KEY".to_string()), FIELD_MODEL_KEY_ENV);

        let err = model
            .complete(CompletionRequest::new("m", vec![Message::user("x")]))
            .await
            .expect_err("401 must fail");
        let ForgeError::Provider(msg) = err else {
            panic!("expected a provider error");
        };
        assert!(msg.contains("401"), "msg: {msg}");
        assert!(msg.contains("OMLX_API_KEY"), "msg: {msg}");
        assert!(msg.contains("remove model_key_env"), "msg: {msg}");
    }

    /// A 401 with no `model_key_env` configured at all is the other half:
    /// the fix is to name one, so the hint says so.
    #[tokio::test]
    async fn unauthorized_error_without_key_env_says_how_to_configure_one() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
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
            .expect_err("401 must fail");
        let ForgeError::Provider(msg) = err else {
            panic!("expected a provider error");
        };
        assert!(msg.contains("model_key_env"), "msg: {msg}");
    }

    /// Non-credential failures must not acquire a credential hint — a 500
    /// is not fixed by exporting a key.
    #[tokio::test]
    async fn non_auth_http_error_carries_no_credential_hint() {
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
        .expect("construct")
        .with_key_env(Some("OMLX_API_KEY".to_string()), FIELD_MODEL_KEY_ENV);
        let err = model
            .complete(CompletionRequest::new("m", vec![Message::user("x")]))
            .await
            .expect_err("500 must fail");
        let ForgeError::Provider(msg) = err else {
            panic!("expected a provider error");
        };
        assert!(!msg.contains("hint:"), "msg: {msg}");
    }

    #[test]
    fn credential_hint_names_the_env_var_and_both_fixes() {
        let hint = credential_hint(Some("OMLX_API_KEY"), FIELD_MODEL_KEY_ENV);
        assert!(hint.starts_with("hint: set OMLX_API_KEY"), "hint: {hint}");
        assert!(hint.contains(".env"), "hint: {hint}");
        assert!(hint.contains("remove model_key_env"), "hint: {hint}");

        let hint = credential_hint(None, FIELD_MODEL_KEY_ENV);
        assert!(
            hint.contains("no model_key_env is configured"),
            "hint: {hint}"
        );
    }

    /// End-to-end for the reported failure: `model_from_config` must hand
    /// the configured env var name to the client, or the 401 hint above
    /// can never fire in a real run.
    #[tokio::test]
    async fn model_from_config_propagates_key_env_into_the_401_hint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_string(r#"{"error":{"message":"API key required"}}"#),
            )
            .mount(&server)
            .await;

        let config = Config {
            model: "Qwen3-Coder-Next-4bit".to_string(),
            model_base_url: Some(server.uri()),
            model_key_env: Some("OMLX_API_KEY".to_string()),
            ..Config::default()
        };
        let model = model_from_config(&config, std::path::Path::new(".")).expect("builds");
        let err = model
            .complete(CompletionRequest::new(
                "Qwen3-Coder-Next-4bit",
                vec![Message::user("Explain this project")],
            ))
            .await
            .expect_err("401 must fail");
        let ForgeError::Provider(msg) = err else {
            panic!("expected a provider error");
        };
        assert!(msg.contains("OMLX_API_KEY"), "msg: {msg}");
        assert!(msg.contains("remove model_key_env"), "msg: {msg}");
    }

    /// When the var came from a `[models]` entry, the hint must not tell the
    /// user to delete `model_key_env` — there is no such line in their file.
    #[tokio::test]
    async fn key_env_from_a_models_entry_points_at_the_entry_not_model_key_env() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
            .mount(&server)
            .await;

        let mut config = Config {
            model: "local-thing".to_string(),
            model_base_url: Some(server.uri()),
            ..Config::default()
        };
        config.models.insert(
            "local-thing".to_string(),
            forge_config::ModelEntry {
                key_env: Some("ENTRY_ONLY_KEY".to_string()),
                ..Default::default()
            },
        );
        let model = model_from_config(&config, std::path::Path::new(".")).expect("builds");
        let err = model
            .complete(CompletionRequest::new(
                "local-thing",
                vec![Message::user("x")],
            ))
            .await
            .expect_err("401 must fail");
        let ForgeError::Provider(msg) = err else {
            panic!("expected a provider error");
        };
        assert!(msg.contains("ENTRY_ONLY_KEY"), "msg: {msg}");
        assert!(msg.contains("the model entry's key_env"), "msg: {msg}");
        assert!(!msg.contains("remove model_key_env"), "msg: {msg}");
    }

    #[test]
    fn model_from_config_mock_is_explicit() {
        let config = Config {
            model: "mock-local".to_string(),
            ..Config::default()
        };
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
    async fn explicit_global_base_url_wins_over_entry_url() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "content": "from override endpoint" } }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        // qwen3-coder's entry URL is 127.0.0.1:8080 — unreachable here. An
        // explicitly changed global base_url must win and reach the mock.
        let config = Config {
            model: "qwen3-coder".to_string(),
            model_base_url: Some(server.uri()),
            ..Config::default()
        };
        let model = model_from_config(&config, std::path::Path::new(".")).expect("builds");
        let response = model
            .complete(CompletionRequest::new(
                "qwen3-coder",
                vec![Message::user("hi")],
            ))
            .await
            .expect("override endpoint answers");
        assert_eq!(response.content, "from override endpoint");
    }

    #[tokio::test]
    async fn missing_key_env_sends_no_auth_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "content": "ok" } }]
            })))
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
        let response = model
            .complete(CompletionRequest::new("m", vec![Message::user("x")]))
            .await
            .expect("completes without auth");
        assert_eq!(response.content, "ok");

        let requests = server.received_requests().await.expect("requests");
        assert!(
            !requests[0]
                .headers
                .keys()
                .any(|k| k.as_str().eq_ignore_ascii_case("authorization")),
            "no Authorization header must be sent"
        );
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
