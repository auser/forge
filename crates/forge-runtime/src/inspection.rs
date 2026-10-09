//! Transport-neutral, read-only inspection and append-only session consent.
//! Construction never creates providers, stores, directories or worker tasks.
use std::{path::PathBuf, sync::Arc};

use forge_config::Config;
use forge_context::{
    ArtifactLimits, ArtifactMetadata, ContextPlan, FsArtifactStore, FsContextStore,
    FsObservationStore, FsObserverQueue, ObservationBatch, ObservationError, ObservationKind,
    ObserverError, ObserverStatus, SourceRange,
};
use forge_core::{Event, EventKind, ForgeError, SessionStore};
use forge_session::JsonlSessionStore;
use serde::Serialize;

use crate::observer::ObserverPrices;

// Serializes local policy publication with the final commitment check. Remote
// processes are detected by polling; a commit already in progress cannot undo.
pub(crate) static MEMORY_POLICY_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Host-supplied outcome of centralized provider/model/egress validation.
/// Reasons must be safe diagnostics, never credential or endpoint contents.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ObserverReadiness {
    Ready,
    Unavailable { reason: String },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
pub enum StoreInspection<T> {
    Missing,
    Available(T),
    Corrupt,
    Unavailable,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CompressionStatus {
    pub decisions: usize,
    pub saved_chars: usize,
    pub saved_estimated_tokens: usize,
}
#[derive(Debug, Clone, Default, Serialize)]
pub struct RetrievalStatus {
    pub attempts: usize,
    pub results: usize,
    pub errors: usize,
}
#[derive(Debug, Clone, Serialize)]
pub struct ContextStatus {
    pub session_id: String,
    pub latest_plan: StoreInspection<ContextPlan>,
    pub compression: CompressionStatus,
    pub retrieval: RetrievalStatus,
    /// Project-wide metadata, not session-local live payload verification.
    pub project_artifact_metadata: StoreInspection<ArtifactMetadata>,
}
#[derive(Debug, Clone, Serialize)]
pub struct MemoryStatus {
    pub session_id: String,
    pub desired_enabled: bool,
    /// Policy eligibility, not dispatch readiness: at least one estimated
    /// unobserved chunk must fit the budget. With none, check minimum priced
    /// token allowance. Leases, retries and future source chunks still require
    /// the worker's atomic reservation check.
    pub effective_eligible: bool,
    pub unavailable_reasons: Vec<String>,
    pub raw_event_count: usize,
    pub session_observations: StoreInspection<usize>,
    pub consolidated_memory: String,
    pub live_prompt_injection: bool,
    pub live_prompt_injection_unavailable_reason: String,
    /// Persisted session-filtered snapshot, not live worker telemetry.
    pub session_jobs: StoreInspection<ObserverStatus>,
    pub project_daily_charged_micro_usd: StoreInspection<u64>,
    pub session_budget_usd: f64,
    pub project_daily_budget_usd: f64,
}
#[derive(Debug, Clone, Serialize)]
pub struct MemoryItem {
    pub id: String,
    pub batch_id: String,
    pub kind: ObservationKind,
    pub content: String,
    pub truncated: bool,
}
#[derive(Debug, Clone, Serialize)]
pub struct MemorySource {
    pub batch_id: String,
    pub source_session_id: String,
    pub range: SourceRange,
    pub source_fingerprint: String,
    pub observer_version: String,
    pub observation_count: usize,
}
#[derive(Debug, Clone, Serialize)]
pub struct Page<T> {
    pub session_id: String,
    pub store: StoreInspection<()>,
    pub items: Vec<T>,
    pub next_offset: Option<usize>,
}
pub type MemoryPage = Page<MemoryItem>;
pub type MemorySourcePage = Page<MemorySource>;

/// Last visible policy wins, including copied parent event IDs at fork cuts.
pub fn memory_observation_enabled(events: &[Event]) -> bool {
    events
        .iter()
        .rev()
        .find_map(|event| match event.kind {
            EventKind::MemoryObservationChanged { enabled } => Some(enabled),
            _ => None,
        })
        .unwrap_or(false)
}

#[derive(Clone)]
pub struct ContextMemoryService {
    config: Arc<Config>,
    sessions: Arc<JsonlSessionStore>,
    root: PathBuf,
    readiness: ObserverReadiness,
}
impl ContextMemoryService {
    pub fn new(
        config: Arc<Config>,
        sessions: Arc<JsonlSessionStore>,
        context_root: PathBuf,
        readiness: ObserverReadiness,
    ) -> Self {
        Self {
            config,
            sessions,
            root: context_root,
            readiness,
        }
    }

    fn events(&self, session: &str) -> Result<Vec<Event>, ForgeError> {
        if session.is_empty()
            || session.len() > 128
            || !session
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(ForgeError::session("invalid session identifier"));
        }
        self.sessions
            .events_for(session)
            .map_err(|_| ForgeError::session("session events unavailable"))
    }

    fn prerequisites(&self) -> Vec<String> {
        let mut reasons = Vec::new();
        if !self.config.observer.enabled {
            reasons.push("project observer is disabled".into());
        }
        let model = self
            .config
            .observer
            .model
            .as_deref()
            .filter(|s| !s.trim().is_empty());
        if model.is_none() {
            reasons.push("observer requires an explicit model".into());
        }
        if self.prices().is_none() {
            reasons.push("observer requires complete known input and output prices".into());
        }
        if [
            self.config.observer.session_usd,
            self.config.observer.daily_usd,
        ]
        .into_iter()
        .any(|v| ObserverPrices::micro_usd(v, false).is_err())
        {
            reasons.push("observer budget is invalid".into());
        }
        if let ObserverReadiness::Unavailable { reason } = &self.readiness {
            reasons.push(reason.clone());
        }
        reasons
    }

    fn prices(&self) -> Option<ObserverPrices> {
        let model = self.config.observer.model.as_deref();
        let entries = self.config.model_entries();
        let catalogue = forge_config::catalogue::load_for_routing(&self.config);
        let book = forge_config::CostBook::new(
            entries,
            catalogue.as_ref().map(|cached| &cached.catalogue),
        );
        let prices = model.and_then(|name| {
            if let Some(entry) = entries.get(name)
                && (entry.cost_input_per_mtok.is_some() || entry.cost_output_per_mtok.is_some())
            {
                return Some((entry.cost_input_per_mtok?, entry.cost_output_per_mtok?));
            }
            book.price(name)
        });
        let (input, output) = prices?;
        Some(ObserverPrices {
            input_micro_usd_per_million: ObserverPrices::micro_usd(input, true).ok()?,
            output_micro_usd_per_million: ObserverPrices::micro_usd(output, true).ok()?,
        })
    }

    fn batches(&self, session: &str, events: &[Event]) -> StoreInspection<Vec<ObservationBatch>> {
        match FsObservationStore::new(&self.root).inspect(session, events, self.sessions.redactor())
        {
            Ok(Some(projection)) => StoreInspection::Available(projection.batches().to_vec()),
            Ok(None) => StoreInspection::Missing,
            Err(
                ObservationError::Corrupt
                | ObservationError::SourceChanged
                | ObservationError::InvalidFork,
            ) => StoreInspection::Corrupt,
            Err(_) => StoreInspection::Unavailable,
        }
    }

    pub fn memory_status(&self, session: &str) -> Result<MemoryStatus, ForgeError> {
        let events = self.events(session)?;
        let desired_enabled = memory_observation_enabled(&events);
        let mut reasons = self.prerequisites();
        if !desired_enabled {
            reasons.push("session consent is off".into());
        }
        let batches = self.batches(session, &events);
        let observations = match &batches {
            StoreInspection::Available(batches) => {
                StoreInspection::Available(batches.iter().map(|b| b.observations.len()).sum())
            }
            StoreInspection::Missing => StoreInspection::Missing,
            StoreInspection::Corrupt => StoreInspection::Corrupt,
            StoreInspection::Unavailable => StoreInspection::Unavailable,
        };
        let jobs = match FsObserverQueue::new(&self.root).inspect_session(session) {
            Ok(Some(status)) => StoreInspection::Available(status),
            Ok(None) => StoreInspection::Missing,
            Err(ObserverError::Corrupt) => StoreInspection::Corrupt,
            Err(_) => StoreInspection::Unavailable,
        };
        if matches!(
            observations,
            StoreInspection::Corrupt | StoreInspection::Unavailable
        ) {
            reasons.push("observation store is unhealthy".into());
        }
        if matches!(
            jobs,
            StoreInspection::Corrupt | StoreInspection::Unavailable
        ) {
            reasons.push("observer queue is unhealthy".into());
        }
        if events
            .iter()
            .any(|event| matches!(event.kind, EventKind::SessionForked { .. }))
            && matches!(observations, StoreInspection::Missing)
        {
            reasons.push("fork observation snapshot is unavailable".into());
        }
        let daily = match FsObserverQueue::new(&self.root).inspect_daily_charged() {
            Ok(Some(charged)) => StoreInspection::Available(charged),
            Ok(None) => StoreInspection::Missing,
            Err(ObserverError::Corrupt) => StoreInspection::Corrupt,
            Err(_) => StoreInspection::Unavailable,
        };
        let reservation = self.inspection_reservation(session, &events, &batches);
        let charged = match &jobs {
            StoreInspection::Missing => Some(0),
            StoreInspection::Available(status) => status
                .known_micro_usd
                .checked_add(status.unknown_micro_usd)
                .and_then(|value| value.checked_add(status.reserved_micro_usd)),
            _ => None,
        };
        let daily_charged = match &daily {
            StoreInspection::Missing => Some(0),
            StoreInspection::Available(charged) => Some(*charged),
            _ => None,
        };
        for (charged, ceiling, reason) in [
            (
                charged,
                self.config.observer.session_usd,
                "session observer budget cannot reserve",
            ),
            (
                daily_charged,
                self.config.observer.daily_usd,
                "project daily observer budget cannot reserve",
            ),
        ] {
            let affordable = charged
                .zip(reservation)
                .and_then(|(charged, reserve)| charged.checked_add(reserve))
                .zip(ObserverPrices::micro_usd(ceiling, false).ok())
                .is_some_and(|(needed, limit)| needed <= limit);
            if !affordable {
                reasons.push(reason.into());
            }
        }
        Ok(MemoryStatus {
            session_id: session.into(),
            desired_enabled,
            effective_eligible: reasons.is_empty(),
            unavailable_reasons: reasons,
            raw_event_count: events.len(),
            session_observations: observations,
            consolidated_memory: "unavailable (CONTEXT-7)".into(),
            live_prompt_injection: false,
            live_prompt_injection_unavailable_reason: "observations are not injected into requests"
                .into(),
            session_jobs: jobs,
            project_daily_charged_micro_usd: daily,
            session_budget_usd: self.config.observer.session_usd,
            project_daily_budget_usd: self.config.observer.daily_usd,
        })
    }

    fn inspection_reservation(
        &self,
        session: &str,
        events: &[Event],
        batches: &StoreInspection<Vec<ObservationBatch>>,
    ) -> Option<u64> {
        let prices = self.prices()?;
        let policy = forge_context::ObserverPolicy {
            observer_version: forge_context::OBSERVER_VERSION.into(),
            model: self.config.observer.model.clone()?,
            prompt_version: forge_context::OBSERVER_PROMPT_VERSION.into(),
            limits: forge_context::ObserverLimits::default(),
        };
        let chunks =
            forge_context::observer_chunks(session, events, self.sessions.redactor(), &policy)
                .ok()?;
        let mut reservation: Option<u64> = None;
        for chunk in chunks {
            if let StoreInspection::Available(batches) = batches
                && batches.iter().any(|batch| {
                    batch.source_session_id == session
                        && batch.observer_version == policy.observer_version
                        && batch.range == chunk.job.range
                        && batch.source_fingerprint == chunk.job.source_fingerprint
                })
            {
                continue;
            }
            let cost = prices.cost(
                chunk.input_tokens,
                u64::from(policy.limits.max_output_tokens),
            )?;
            reservation = Some(reservation.map_or(cost, |minimum| minimum.min(cost)));
        }
        // A large blocked chunk must not hide an affordable smaller chunk.
        // With no pending source, require room for at least one priced token.
        // Zero-price work remains affordable exactly at the ceiling.
        reservation.or_else(|| prices.cost(1, 1))
    }

    /// Allows an absent safe current-chat ID; CLI hosts must check existence.
    /// Appends through the session redaction path; never synthesizes a model run.
    pub fn set_memory_enabled(
        &self,
        session: &str,
        enabled: bool,
    ) -> Result<MemoryStatus, ForgeError> {
        self.events(session)?;
        if enabled {
            let reasons = self.prerequisites();
            if !reasons.is_empty() {
                return Err(ForgeError::config(reasons.join("; ")));
            }
        }
        {
            let _gate = MEMORY_POLICY_GATE
                .lock()
                .map_err(|_| ForgeError::session("memory policy unavailable"))?;
            self.sessions.append(Event::new(
                forge_session::new_run_id(),
                session,
                EventKind::MemoryObservationChanged { enabled },
            ))?;
        }
        self.memory_status(session)
    }

    pub fn context_status(&self, session: &str) -> Result<ContextStatus, ForgeError> {
        let events = self.events(session)?;
        let mut latest_plan = StoreInspection::Missing;
        let store = FsContextStore::new(&self.root);
        for event in events.iter().rev() {
            if let EventKind::ContextPlanRecorded {
                plan_id,
                request_ordinal,
                ..
            } = &event.kind
            {
                // Never follow event.plan_path. IDs must match the typed source.
                if plan_id != &format!("{}/{}", event.run_id, request_ordinal) {
                    latest_plan = StoreInspection::Corrupt;
                    continue;
                }
                match store.inspect_plan(&event.run_id, *request_ordinal) {
                    Ok(Some(plan))
                        if plan.id == *plan_id
                            && plan.run_id == event.run_id
                            && plan.session_id == event.session_id
                            && plan.request_ordinal == *request_ordinal
                            && plan.version == forge_context::CONTEXT_PLAN_VERSION =>
                    {
                        latest_plan = StoreInspection::Available(plan);
                        break;
                    }
                    Ok(Some(_)) => latest_plan = StoreInspection::Corrupt,
                    Ok(None) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                        latest_plan = StoreInspection::Corrupt
                    }
                    Err(_) => latest_plan = StoreInspection::Unavailable,
                }
            }
        }
        let mut compression = CompressionStatus::default();
        let mut retrieval = RetrievalStatus::default();
        for event in &events {
            match &event.kind {
                EventKind::ToolOutputCompression { baseline, view, .. } => {
                    compression.decisions += 1;
                    compression.saved_chars = compression
                        .saved_chars
                        .saturating_add(baseline.chars.saturating_sub(view.chars));
                    compression.saved_estimated_tokens =
                        compression.saved_estimated_tokens.saturating_add(
                            baseline
                                .estimated_tokens
                                .saturating_sub(view.estimated_tokens),
                        );
                }
                EventKind::ToolCallRequested { tool, .. } if tool == "retrieve_tool_output" => {
                    retrieval.attempts += 1
                }
                EventKind::ToolResult { tool, is_error, .. } if tool == "retrieve_tool_output" => {
                    retrieval.results += 1;
                    retrieval.errors += usize::from(*is_error);
                }
                _ => {}
            }
        }
        let limits = &self.config.context_artifacts;
        let artifacts = FsArtifactStore::new(
            &self.root,
            ArtifactLimits {
                max_project_bytes: limits.max_project_bytes,
                max_age_secs: limits.max_age_secs,
                max_artifact_bytes: limits.max_artifact_bytes,
            },
        );
        let project_artifact_metadata = match artifacts.inspect() {
            Ok(Some(metadata)) => StoreInspection::Available(metadata),
            Ok(None) => StoreInspection::Missing,
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                StoreInspection::Corrupt
            }
            Err(_) => StoreInspection::Unavailable,
        };
        Ok(ContextStatus {
            session_id: session.into(),
            latest_plan,
            compression,
            retrieval,
            project_artifact_metadata,
        })
    }

    pub fn memory_show(&self, session: &str, offset: usize) -> Result<MemoryPage, ForgeError> {
        let events = self.events(session)?;
        // Inspection follows immutable ledger commit order (including the frozen
        // fork prefix), then record order within each batch. Unlike the canonical
        // renderer's source order, appending an earlier source cannot move offsets.
        let (store, batches) = split(self.batches(session, &events));
        let items = batches
            .into_iter()
            .flat_map(|b| {
                b.observations.into_iter().map(move |o| MemoryItem {
                    id: o.id,
                    batch_id: b.id.clone(),
                    kind: o.kind,
                    content: o.content,
                    truncated: false,
                })
            })
            .collect::<Vec<_>>();
        // Bound each content item too: JSON escaping can expand UTF-8 bytes.
        let items = items
            .into_iter()
            .map(|mut item| {
                while serde_json::to_vec(&item).map_or(usize::MAX, |b| b.len()) > 14 * 1024 {
                    let cut = item
                        .content
                        .char_indices()
                        .nth(item.content.chars().count() / 2)
                        .map_or(0, |(i, _)| i);
                    item.content.truncate(cut);
                    item.truncated = true;
                }
                item
            })
            .collect();
        Ok(page(session, store, items, offset))
    }

    pub fn memory_sources(
        &self,
        session: &str,
        offset: usize,
    ) -> Result<MemorySourcePage, ForgeError> {
        let events = self.events(session)?;
        // Same append-stable ledger order as memory_show, not renderer order.
        let (store, batches) = split(self.batches(session, &events));
        let items = batches
            .into_iter()
            .map(|b| MemorySource {
                batch_id: b.id,
                source_session_id: b.source_session_id,
                range: b.range,
                source_fingerprint: b.source_fingerprint,
                observer_version: b.observer_version,
                observation_count: b.observations.len(),
            })
            .collect();
        Ok(page(session, store, items, offset))
    }
}

fn split<T>(value: StoreInspection<Vec<T>>) -> (StoreInspection<()>, Vec<T>) {
    match value {
        StoreInspection::Available(items) => (StoreInspection::Available(()), items),
        StoreInspection::Missing => (StoreInspection::Missing, vec![]),
        StoreInspection::Corrupt => (StoreInspection::Corrupt, vec![]),
        StoreInspection::Unavailable => (StoreInspection::Unavailable, vec![]),
    }
}
fn page<T: Serialize>(
    session: &str,
    store: StoreInspection<()>,
    items: Vec<T>,
    offset: usize,
) -> Page<T> {
    let total = items.len();
    let mut page = Page {
        session_id: session.into(),
        store,
        items: vec![],
        next_offset: Some(offset),
    };
    for item in items.into_iter().skip(offset).take(20) {
        page.items.push(item);
        page.next_offset = Some(offset.saturating_add(page.items.len()));
        if serde_json::to_vec(&page).map_or(usize::MAX, |b| b.len()) > 16 * 1024 {
            page.items.pop();
            break;
        }
    }
    let next = offset.saturating_add(page.items.len());
    page.next_offset = (next < total).then_some(next);
    page
}

#[cfg(test)]
mod tests;
