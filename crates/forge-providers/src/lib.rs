//! Model providers (`MockModel`, scripted mock, OpenAI-compatible HTTP) and
//! decision routers (static, mock, System One-compatible HTTP, fallback).

mod anthropic;
mod credentials;
mod model;
mod router;
mod scripted;

pub use anthropic::AnthropicModel;
pub use credentials::{
    AuthProbe, CredentialKind, CredentialSource, ResolvedCredential, codex_auth, probe_auth,
    resolve_credential,
};
pub use model::{MockModel, OpenAiCompatibleModel, model_from_config};
pub use router::{
    CheapestRouter, FallbackRouter, HttpRouter, LayaRouter, MockRouter, StaticRouter,
    ThresholdRouter, filter_candidates, router_from_config,
};
pub use scripted::{ScriptedMockModel, ScriptedReply, scripted_mock_from_path};
