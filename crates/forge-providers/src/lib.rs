//! Model providers (`MockModel`, scripted mock, OpenAI-compatible HTTP) and
//! decision routers (static, mock, System One-compatible HTTP, fallback).

mod model;
mod router;
mod scripted;

pub use model::{MockModel, OpenAiCompatibleModel, model_from_config};
pub use router::{
    FallbackRouter, HttpRouter, MockRouter, StaticRouter, filter_candidates, router_from_config,
};
pub use scripted::{ScriptedMockModel, ScriptedReply, scripted_mock_from_path};
