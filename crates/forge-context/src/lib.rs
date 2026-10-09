//! Provider-neutral context accounting and sanitized tool artifact storage.
//!
//! Plans deliberately contain measurements and hashes only. They never retain
//! the message or tool-schema content used to derive those measurements.
//! Artifacts separately preserve sanitized complete oversized tool outputs,
//! accessible only through explicit source-authorized bounded retrieval.

mod artifact;
mod compression;
mod observation;
mod observation_render;
mod plan;
mod store;

pub use artifact::{
    ArtifactClock, ArtifactLimits, ArtifactQuery, ArtifactRead, ArtifactRef, ArtifactSource,
    ArtifactStore, FsArtifactStore, MAX_ARTIFACT_MANIFESTS, MAX_ARTIFACT_OBJECTS,
    MAX_ARTIFACT_RESPONSE_BYTES, MemoryArtifactStore, REDACTION_POLICY_VERSION,
    SanitizedArtifactSource, SanitizedOutput,
};
pub use compression::{
    COMPRESSION_VERSION, CompressionDecision, CompressionKind, CompressionReason,
    CompressionResult, compress_tool_output,
};
pub use observation::{
    FsObservationStore, LedgerProjection, MAX_OBSERVATION_BATCHES, MAX_OBSERVATION_CONTENT_BYTES,
    MAX_OBSERVATION_LEDGER_BYTES, MAX_OBSERVATION_SOURCE_EVENTS, MAX_OBSERVATIONS_PER_BATCH,
    MemoryObservationStore, Observation, ObservationBatch, ObservationDraft, ObservationError,
    ObservationKind, ObservationScope, ObservationStore, SourceRange, ValidatedObservationBatch,
};
pub use observation_render::{
    OBSERVATION_HEADER, ObservationRender, ObservationSelection, ObservationSelectionReason,
    render_observations,
};
pub use plan::{
    CONTEXT_PLAN_VERSION, ContextComponents, ContextPlan, ContextPlanDraft, ContextPlanSummary,
    ContextSize, StablePrefix, stable_prefix,
};
pub use store::{ContextStore, FsContextStore, MemoryContextStore};
