//! Model providers (OpenAI-compatible HTTP, Anthropic, and test-only
//! mocks) and decision routers (needle, jev, laya, http, static, cheapest,
//! and a test-only mock).
//!
//! The mocks are gated: configuration can only select them when
//! `FORGE_TEST_MOCKS=1` is set (see `forge_config::test_mocks`).
//! Constructing them directly from Rust — which is what unit tests across
//! the workspace do — is unaffected.

mod anthropic;
mod credentials;
mod jev;
mod local_only;
mod model;
mod router;
mod scripted;

pub use anthropic::AnthropicModel;
pub use credentials::{
    AuthProbe, CredentialKind, CredentialSource, ResolvedCredential, codex_auth, probe_auth,
    resolve_credential,
};
pub use jev::JevRouter;
pub use local_only::{EgressPolicy, endpoint_is_local};
pub use model::{
    MockModel, OpenAiCompatibleModel, is_mock_model, model_endpoint, model_from_config,
};
pub use router::{
    CheapestRouter, FallbackRouter, HttpRouter, LayaRouter, MockRouter, StaticRouter,
    ThresholdRouter, filter_candidates, jev_credential_present, resolved_jev_key_env,
    resolved_jev_url, router_endpoint, router_from_config,
};
pub use scripted::{ScriptedMockModel, ScriptedReply, scripted_mock_from_path};
