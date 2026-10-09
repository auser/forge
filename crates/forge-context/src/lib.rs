//! Provider-neutral context accounting.
//!
//! Plans deliberately contain measurements and hashes only. They never retain
//! the message or tool-schema content used to derive those measurements.

mod plan;
mod store;

pub use plan::{
    CONTEXT_PLAN_VERSION, ContextComponents, ContextPlan, ContextPlanDraft, ContextPlanSummary,
    ContextSize, StablePrefix, stable_prefix,
};
pub use store::{ContextStore, FsContextStore, MemoryContextStore};
