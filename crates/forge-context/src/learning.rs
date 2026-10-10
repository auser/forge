//! Deterministic, reviewable learning proposals derived from redacted events.
//!
//! Analysis never edits guidance. Proposals remain in ignored local context
//! storage until a separate explicit decision changes their status.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use chrono::{DateTime, Utc};
use forge_core::{Event, EventKind};
use forge_session::Redactor;
use serde::{Deserialize, Serialize};

use crate::{artifact::ArtifactFiles, observation::hash};

pub const LEARNING_VERSION: u32 = 1;
const MAX_INDEX_BYTES: usize = 4 * 1024 * 1024;
const MAX_PROPOSALS: usize = 1_024;
const MAX_ANALYZED_EVENTS: usize = 100_000;
const MAX_SOURCES_PER_PROPOSAL: usize = 32;
const MAX_SIGNAL_BYTES: usize = 512;
const MAX_RECOMMENDATION_BYTES: usize = 1_024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LearningError {
    Invalid,
    Corrupt,
    Unavailable,
    NotFound,
    LimitExceeded,
}
impl std::fmt::Display for LearningError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Invalid => "invalid learning request",
            Self::Corrupt => "learning proposal storage is corrupt",
            Self::Unavailable => "learning proposal storage is unavailable",
            Self::NotFound => "learning proposal not found",
            Self::LimitExceeded => "learning analysis limit exceeded",
        })
    }
}
impl std::error::Error for LearningError {}
type Result<T> = std::result::Result<T, LearningError>;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    Failure,
    Correction,
    RepeatedInstruction,
    RepeatedRetrieval,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    Pending,
    Accepted,
    Rejected,
}

#[derive(Clone, Debug, Default)]
pub struct LearningScope {
    session: Option<String>,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
}
impl LearningScope {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn session(mut self, session: impl Into<String>) -> Self {
        self.session = Some(session.into());
        self
    }
    pub fn since(mut self, since: DateTime<Utc>) -> Self {
        self.since = Some(since);
        self
    }
    pub fn until(mut self, until: DateTime<Utc>) -> Self {
        self.until = Some(until);
        self
    }
    pub fn session_id(&self) -> Option<&str> {
        self.session.as_deref()
    }
    fn validate(&self) -> Result<()> {
        if self
            .session
            .as_deref()
            .is_some_and(|session| !crate::observation::identifier(session))
            || self
                .since
                .zip(self.until)
                .is_some_and(|(since, until)| since > until)
        {
            return Err(LearningError::Invalid);
        }
        Ok(())
    }
    fn includes(&self, container_session: &str, event: &Event) -> bool {
        self.session
            .as_deref()
            .is_none_or(|session| session == container_session)
            && self.since.is_none_or(|since| event.ts >= since)
            && self.until.is_none_or(|until| event.ts <= until)
    }
}

#[derive(Clone, Debug)]
pub struct LearningSession {
    pub session_id: String,
    pub events: Vec<Event>,
}
impl LearningSession {
    pub fn new(session_id: impl Into<String>, events: Vec<Event>) -> Self {
        Self {
            session_id: session_id.into(),
            events,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LearningSource {
    pub session_id: String,
    pub run_id: String,
    pub event_seq: u64,
    pub event_type: String,
    pub occurred_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LearningProposal {
    pub id: String,
    pub kind: EvidenceKind,
    pub recommendation: String,
    pub occurrences: usize,
    pub distinct_sessions: usize,
    pub recurrence: String,
    pub sources: Vec<LearningSource>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LearningRecord {
    pub proposal: LearningProposal,
    pub status: ProposalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct LearningMetrics {
    pub pending: usize,
    pub accepted: usize,
    pub rejected: usize,
    pub recurrence_after_acceptance: usize,
    pub accepted_with_recurrence: usize,
}

#[derive(Clone, Debug)]
struct Candidate {
    kind: EvidenceKind,
    normalized: String,
    display: String,
    sources: Vec<LearningSource>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    proposal: LearningProposal,
    signature: String,
    status: ProposalStatus,
    #[serde(default)]
    decided_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    version: u32,
    proposals: BTreeMap<String, Entry>,
}
impl Default for Index {
    fn default() -> Self {
        Self {
            version: LEARNING_VERSION,
            proposals: BTreeMap::new(),
        }
    }
}

pub struct FsLearningStore {
    root: PathBuf,
}
impl FsLearningStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn propose(
        &self,
        sessions: &[LearningSession],
        scope: &LearningScope,
        redactor: &Redactor,
    ) -> Result<Vec<LearningRecord>> {
        let candidates = analyze(sessions, scope, redactor)?;
        let proposals: Vec<_> = candidates
            .values()
            .filter(|candidate| candidate.sources.len() >= 2)
            .map(proposal_from_candidate)
            .collect::<Result<_>>()?;
        self.transaction(&mut |files| {
            let mut index = load(files)?;
            let mut records = Vec::new();
            for (signature, proposal) in &proposals {
                if let Some(existing) = index.proposals.get_mut(&proposal.id) {
                    if existing.signature != *signature || existing.proposal.kind != proposal.kind {
                        return Err(LearningError::Corrupt);
                    }
                    // An accepted recommendation is immutable without a new
                    // review decision. Only its evidence accounting grows.
                    existing.proposal.occurrences = proposal.occurrences;
                    existing.proposal.distinct_sessions = proposal.distinct_sessions;
                    existing
                        .proposal
                        .recurrence
                        .clone_from(&proposal.recurrence);
                    existing.proposal.sources.clone_from(&proposal.sources);
                    records.push(record(existing));
                } else {
                    if index.proposals.len() == MAX_PROPOSALS {
                        return Err(LearningError::LimitExceeded);
                    }
                    let entry = Entry {
                        proposal: proposal.clone(),
                        signature: signature.clone(),
                        status: ProposalStatus::Pending,
                        decided_at: None,
                    };
                    records.push(record(&entry));
                    index.proposals.insert(proposal.id.clone(), entry);
                }
            }
            save(files, &index)?;
            records.sort_by(|a, b| a.proposal.id.cmp(&b.proposal.id));
            Ok(records)
        })
    }

    pub fn list(&self) -> Result<Vec<LearningRecord>> {
        let Some(index) = self.inspect()? else {
            return Ok(Vec::new());
        };
        Ok(index.proposals.values().map(record).collect())
    }

    pub fn get(&self, id: &str) -> Result<LearningRecord> {
        if !valid_hash(id) {
            return Err(LearningError::Invalid);
        }
        self.inspect()?
            .and_then(|index| index.proposals.get(id).map(record))
            .ok_or(LearningError::NotFound)
    }

    pub fn decide(
        &self,
        id: &str,
        status: ProposalStatus,
        decided_at: DateTime<Utc>,
    ) -> Result<LearningRecord> {
        if !valid_hash(id) || status == ProposalStatus::Pending {
            return Err(LearningError::Invalid);
        }
        self.transaction(&mut |files| {
            let mut index = load(files)?;
            let entry = index.proposals.get_mut(id).ok_or(LearningError::NotFound)?;
            if entry.status != ProposalStatus::Pending && entry.status != status {
                return Err(LearningError::Invalid);
            }
            if entry.status == ProposalStatus::Pending {
                entry.status = status;
                entry.decided_at = Some(decided_at);
            }
            let result = record(entry);
            save(files, &index)?;
            Ok(result)
        })
    }

    pub fn accepted_guidance(&self) -> Result<Vec<LearningRecord>> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|record| record.status == ProposalStatus::Accepted)
            .collect())
    }

    pub fn metrics(
        &self,
        sessions: &[LearningSession],
        scope: &LearningScope,
        redactor: &Redactor,
    ) -> Result<LearningMetrics> {
        let candidates = analyze(sessions, scope, redactor)?;
        let records = self.list()?;
        let mut metrics = LearningMetrics {
            pending: 0,
            accepted: 0,
            rejected: 0,
            recurrence_after_acceptance: 0,
            accepted_with_recurrence: 0,
        };
        let index = self.inspect()?.unwrap_or_default();
        for record in records {
            match record.status {
                ProposalStatus::Pending => metrics.pending += 1,
                ProposalStatus::Rejected => metrics.rejected += 1,
                ProposalStatus::Accepted => {
                    metrics.accepted += 1;
                    let entry = index
                        .proposals
                        .get(&record.proposal.id)
                        .ok_or(LearningError::Corrupt)?;
                    let accepted_at = entry.decided_at.ok_or(LearningError::Corrupt)?;
                    let recurrence = candidates.get(&entry.signature).map_or(0, |candidate| {
                        candidate
                            .sources
                            .iter()
                            .filter(|source| source.occurred_at > accepted_at)
                            .count()
                    });
                    metrics.recurrence_after_acceptance += recurrence;
                    metrics.accepted_with_recurrence += usize::from(recurrence > 0);
                }
            }
        }
        Ok(metrics)
    }

    fn inspect(&self) -> Result<Option<Index>> {
        let Some(bytes) = crate::store::namespace_read(&self.root, "learning", MAX_INDEX_BYTES)
            .map_err(|_| LearningError::Unavailable)?
        else {
            return Ok(None);
        };
        let index = serde_json::from_slice(&bytes).map_err(|_| LearningError::Corrupt)?;
        validate(&index)?;
        Ok(Some(index))
    }

    fn transaction<T>(
        &self,
        operation: &mut dyn FnMut(&mut dyn ArtifactFiles) -> Result<T>,
    ) -> Result<T> {
        crate::store::learning_transaction(&self.root, &mut |files| Ok(operation(files)))
            .map_err(|_| LearningError::Unavailable)?
    }
}

fn analyze(
    sessions: &[LearningSession],
    scope: &LearningScope,
    redactor: &Redactor,
) -> Result<BTreeMap<String, Candidate>> {
    scope.validate()?;
    let mut candidates: BTreeMap<String, Candidate> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    let mut count = 0usize;
    for session in sessions {
        if !crate::observation::identifier(&session.session_id) {
            return Err(LearningError::Invalid);
        }
        for event in &session.events {
            if !scope.includes(&session.session_id, event) {
                continue;
            }
            count = count.checked_add(1).ok_or(LearningError::LimitExceeded)?;
            if count > MAX_ANALYZED_EVENTS {
                return Err(LearningError::LimitExceeded);
            }
            let Some((kind, raw, event_type)) = signal(event) else {
                continue;
            };
            let display = safe_text(redactor, &raw);
            if display.is_empty() {
                continue;
            }
            let normalized = normalize(&display);
            if normalized.is_empty() {
                continue;
            }
            let signature = hash("forge-learning-signal-v1", &(kind, &normalized))
                .map_err(|_| LearningError::Corrupt)?;
            let source = LearningSource {
                session_id: event.session_id.clone(),
                run_id: event.run_id.clone(),
                event_seq: event.seq,
                event_type: event_type.into(),
                occurred_at: event.ts,
            };
            let source_key = format!(
                "{}:{}:{}:{}",
                source.session_id, source.run_id, source.event_seq, source.event_type
            );
            if !seen.insert((signature.clone(), source_key)) {
                continue;
            }
            candidates
                .entry(signature)
                .and_modify(|candidate| candidate.sources.push(source.clone()))
                .or_insert(Candidate {
                    kind,
                    normalized,
                    display,
                    sources: vec![source],
                });
        }
    }
    Ok(candidates)
}

fn signal(event: &Event) -> Option<(EvidenceKind, String, &'static str)> {
    match &event.kind {
        EventKind::Error { message } => Some((EvidenceKind::Failure, message.clone(), "error")),
        EventKind::ToolCompleted {
            name,
            success: false,
        } => Some((
            EvidenceKind::Failure,
            format!("tool {name} failed"),
            "tool_completed",
        )),
        EventKind::ToolResult {
            tool,
            output,
            is_error: true,
            ..
        } => Some((
            EvidenceKind::Failure,
            format!("tool {tool}: {}", output.lines().next().unwrap_or("error")),
            "tool_result",
        )),
        EventKind::ToolCallRequested { tool, .. } if tool == "retrieve_tool_output" => Some((
            EvidenceKind::RepeatedRetrieval,
            "retrieve complete sanitized tool output before acting".into(),
            "tool_call_requested",
        )),
        EventKind::InputReceived { message } => {
            let normalized = normalize(message);
            let kind = if is_correction(&normalized) {
                EvidenceKind::Correction
            } else {
                EvidenceKind::RepeatedInstruction
            };
            Some((kind, message.clone(), "input_received"))
        }
        EventKind::Note { message } => {
            let normalized = normalize(message);
            let kind = if is_correction(&normalized) {
                EvidenceKind::Correction
            } else {
                EvidenceKind::RepeatedInstruction
            };
            Some((kind, message.clone(), "note"))
        }
        _ => None,
    }
}

fn proposal_from_candidate(candidate: &Candidate) -> Result<(String, LearningProposal)> {
    let signature = hash(
        "forge-learning-signal-v1",
        &(candidate.kind, &candidate.normalized),
    )
    .map_err(|_| LearningError::Corrupt)?;
    let sessions: BTreeSet<_> = candidate
        .sources
        .iter()
        .map(|source| source.session_id.as_str())
        .collect();
    let recommendation = truncate_owned(
        match candidate.kind {
        EvidenceKind::Failure => format!(
            "Prevent or recover from this recurring failure: {}",
            candidate.display
        ),
        EvidenceKind::Correction => format!(
            "Honor this recurring developer correction: {}",
            candidate.display
        ),
        EvidenceKind::RepeatedInstruction => format!(
            "Follow this recurring developer instruction: {}",
            candidate.display
        ),
        EvidenceKind::RepeatedRetrieval => {
            "Retrieve complete sanitized tool output before acting when context was abbreviated."
                .into()
        }
        },
        MAX_RECOMMENDATION_BYTES,
    );
    let id = hash(
        "forge-learning-proposal-v1",
        &(candidate.kind, &signature, &recommendation),
    )
    .map_err(|_| LearningError::Corrupt)?;
    Ok((
        signature,
        LearningProposal {
            id,
            kind: candidate.kind,
            recommendation,
            occurrences: candidate.sources.len(),
            distinct_sessions: sessions.len(),
            recurrence: recurrence_text(candidate.kind, candidate.sources.len(), sessions.len()),
            sources: candidate
                .sources
                .iter()
                .take(MAX_SOURCES_PER_PROPOSAL)
                .cloned()
                .collect(),
        },
    ))
}

fn recurrence_text(kind: EvidenceKind, occurrences: usize, sessions: usize) -> String {
    let noun = match kind {
        EvidenceKind::Failure => "failure",
        EvidenceKind::Correction => "correction",
        EvidenceKind::RepeatedInstruction => "instruction",
        EvidenceKind::RepeatedRetrieval => "retrieval request",
    };
    format!("{occurrences} recurring {noun} events across {sessions} source session(s)")
}

fn safe_text(redactor: &Redactor, raw: &str) -> String {
    truncate_owned(redactor.redact(raw).trim().to_owned(), MAX_SIGNAL_BYTES)
}

fn normalize(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn is_correction(normalized: &str) -> bool {
    [
        "actually ",
        "no,",
        "instead ",
        "don't ",
        "do not ",
        "please don't ",
        "always ",
        "never ",
    ]
    .iter()
    .any(|prefix| normalized.starts_with(prefix))
}

fn truncate_owned(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}

fn record(entry: &Entry) -> LearningRecord {
    LearningRecord {
        proposal: entry.proposal.clone(),
        status: entry.status,
        decided_at: entry.decided_at,
    }
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn load(files: &dyn ArtifactFiles) -> Result<Index> {
    let index = match files
        .read("index.json", MAX_INDEX_BYTES)
        .map_err(|_| LearningError::Unavailable)?
    {
        Some(bytes) => serde_json::from_slice(&bytes).map_err(|_| LearningError::Corrupt)?,
        None => Index::default(),
    };
    validate(&index)?;
    Ok(index)
}

fn save(files: &mut dyn ArtifactFiles, index: &Index) -> Result<()> {
    validate(index)?;
    let bytes = serde_json::to_vec(index).map_err(|_| LearningError::Corrupt)?;
    if bytes.len() > MAX_INDEX_BYTES {
        return Err(LearningError::LimitExceeded);
    }
    files
        .write("index.json", &bytes)
        .map_err(|_| LearningError::Unavailable)
}

fn validate(index: &Index) -> Result<()> {
    if index.version != LEARNING_VERSION || index.proposals.len() > MAX_PROPOSALS {
        return Err(LearningError::Corrupt);
    }
    for (id, entry) in &index.proposals {
        let proposal = &entry.proposal;
        let expected_id = hash(
            "forge-learning-proposal-v1",
            &(proposal.kind, &entry.signature, &proposal.recommendation),
        )
        .map_err(|_| LearningError::Corrupt)?;
        let unique_sources: BTreeSet<_> = proposal
            .sources
            .iter()
            .map(|source| {
                (
                    &source.session_id,
                    &source.run_id,
                    source.event_seq,
                    &source.event_type,
                )
            })
            .collect();
        if id != &proposal.id
            || !valid_hash(id)
            || expected_id != *id
            || !valid_hash(&entry.signature)
            || proposal.recommendation.trim().is_empty()
            || proposal.recommendation.len() > MAX_RECOMMENDATION_BYTES
            || proposal.occurrences < 2
            || proposal.distinct_sessions == 0
            || proposal.distinct_sessions > proposal.occurrences
            || proposal.recurrence.len() > 256
            || proposal.recurrence
                != recurrence_text(
                    proposal.kind,
                    proposal.occurrences,
                    proposal.distinct_sessions,
                )
            || proposal.sources.is_empty()
            || proposal.sources.len() > MAX_SOURCES_PER_PROPOSAL
            || proposal.sources.len() > proposal.occurrences
            || unique_sources.len() != proposal.sources.len()
            || proposal.sources.iter().any(|source| {
                !crate::observation::identifier(&source.session_id)
                    || !crate::observation::identifier(&source.run_id)
                    || source.event_seq == 0
                    || !matches!(
                        source.event_type.as_str(),
                        "error"
                            | "tool_completed"
                            | "tool_result"
                            | "tool_call_requested"
                            | "input_received"
                            | "note"
                    )
            })
            || matches!(entry.status, ProposalStatus::Pending) != entry.decided_at.is_none()
        {
            return Err(LearningError::Corrupt);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
