//! Model providers (`MockModel`, OpenAI-compatible HTTP) and decision
//! routers (static, mock, System One-compatible HTTP, fallback wrapper).

mod model;
mod router;

pub use model::{MockModel, OpenAiCompatibleModel, model_from_config};
pub use router::{
    FallbackRouter, HttpRouter, MockRouter, StaticRouter, filter_candidates, router_from_config,
};
