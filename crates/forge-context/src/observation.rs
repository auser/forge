//! Source-anchored derived data, never an instruction tier.
//!
//! The bounded index is physically replaced atomically under a permanent native
//! lock. Logically commits only append batches: existing batch prefixes are
//! immutable. Supplied event snapshots are trusted caller input, not disk proof.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Mutex,
};

use forge_core::{EVENT_SCHEMA_VERSION, Event, EventKind};
use forge_session::Redactor;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MAX_OBSERVATION_CONTENT_BYTES: usize = 16 * 1024;
pub const MAX_OBSERVATIONS_PER_BATCH: usize = 128;
pub const MAX_OBSERVATION_BATCHES: usize = 4096;
pub const MAX_OBSERVATION_LEDGER_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_OBSERVATION_SOURCE_EVENTS: usize = 65536;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservationError {
    InvalidSource,
    InvalidMetadata,
    InvalidContent,
    LimitExceeded,
    Overlap,
    SourceChanged,
    InvalidFork,
    Corrupt,
    Unavailable,
}
impl std::fmt::Display for ObservationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "observation operation failed: {self:?}")
    }
}
impl std::error::Error for ObservationError {}
type Result<T> = std::result::Result<T, ObservationError>;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceRange {
    pub start: u64,
    pub end: u64,
}
impl SourceRange {
    pub fn overlaps(self, other: Self) -> bool {
        self.start <= other.end && other.start <= self.end
    }
    fn valid(self, len: usize) -> bool {
        self.start > 0 && self.start <= self.end && self.end <= len as u64
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObservationScope {
    Session,
    Project,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObservationKind {
    Decision,
    Constraint,
    Outcome,
    Question,
    State,
}
pub struct ObservationDraft {
    pub scope: ObservationScope,
    pub kind: ObservationKind,
    pub content: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub id: String,
    pub scope: ObservationScope,
    pub kind: ObservationKind,
    pub content: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ObservationBatch {
    pub id: String,
    pub source_session_id: String,
    pub range: SourceRange,
    pub source_fingerprint: String,
    pub observer_version: String,
    pub created_at: String,
    pub observations: Vec<Observation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observer_job_id: Option<String>,
}
/// Only the redacting, source-validating constructor can cross the commit boundary.
pub struct ValidatedObservationBatch {
    pub(crate) batch: ObservationBatch,
    snapshot: Snapshot,
}

pub(crate) fn identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
}
fn metadata(s: &str, redactor: &Redactor) -> Result<()> {
    if !identifier(s) || redactor.redact(s) != s {
        return Err(ObservationError::InvalidMetadata);
    }
    Ok(())
}
pub(crate) fn hash<T: Serialize + ?Sized>(domain: &str, value: &T) -> Result<String> {
    let bytes = serde_json::to_vec(value).map_err(|_| ObservationError::InvalidSource)?;
    let mut h = Sha256::new();
    h.update(domain.as_bytes());
    h.update([0]);
    h.update(bytes);
    Ok(format!("{:x}", h.finalize()))
}

/// Stop serialization at the bound, rather than allocating an unbounded event
/// and rejecting it afterwards. Errors never retain serializer/source details.
fn source_bytes(event: &Event) -> Result<Vec<u8>> {
    struct Bounded(Vec<u8>);
    impl std::io::Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > (1024 * 1024usize).saturating_sub(self.0.len()) {
                return Err(std::io::ErrorKind::FileTooLarge.into());
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut output = Bounded(Vec::new());
    serde_json::to_writer(&mut output, event).map_err(|error| {
        if error.is_io() {
            ObservationError::LimitExceeded
        } else {
            ObservationError::InvalidSource
        }
    })?;
    Ok(output.0)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Snapshot {
    hashes: Vec<String>,
    pub(crate) local_start: u64,
}
pub(crate) fn snapshot(session: &str, events: &[Event], redactor: &Redactor) -> Result<Snapshot> {
    metadata(session, redactor)?;
    if events.len() > MAX_OBSERVATION_SOURCE_EVENTS {
        return Err(ObservationError::LimitExceeded);
    }
    let mut hashes = Vec::with_capacity(events.len());
    let mut local_start = 1;
    let mut source_size = 0usize;
    // Fork markers describe copied prefixes. The last marker establishes the
    // current log identity; event.session_id on copied events is not that identity.
    for (i, event) in events.iter().enumerate() {
        if event.v == 0 || event.v > EVENT_SCHEMA_VERSION {
            return Err(ObservationError::InvalidSource);
        }
        if let EventKind::RoutingDecisionMade { confidence, .. } = &event.kind
            && !confidence.is_finite()
        {
            return Err(ObservationError::InvalidSource);
        }
        if let EventKind::SessionForked {
            from_session,
            at_position,
        } = &event.kind
        {
            metadata(from_session, redactor)?;
            if *at_position != i as u64 || event.session_id == *from_session {
                return Err(ObservationError::InvalidFork);
            }
            local_start = i as u64 + 2;
        }
        // Bound before hashing. Serialization rejects unsupported non-UTF8 paths.
        let bytes = source_bytes(event)?;
        source_size = source_size.saturating_add(bytes.len());
        if source_size > MAX_OBSERVATION_LEDGER_BYTES {
            return Err(ObservationError::LimitExceeded);
        }
        let mut digest = Sha256::new();
        digest.update(b"forge-source-event-v1\0");
        digest.update(bytes);
        hashes.push(format!("{:x}", digest.finalize()));
    }
    if events
        .iter()
        .skip(local_start.saturating_sub(1) as usize)
        .any(|e| e.session_id != session)
        || (local_start > 1 && events[(local_start - 2) as usize].session_id != session)
    {
        return Err(ObservationError::InvalidSource);
    }
    Ok(Snapshot {
        hashes,
        local_start,
    })
}
pub(crate) fn fingerprint(snapshot: &Snapshot, range: SourceRange) -> Result<String> {
    if !range.valid(snapshot.hashes.len()) {
        return Err(ObservationError::InvalidSource);
    }
    hash(
        "forge-source-range-v1",
        &snapshot.hashes[(range.start - 1) as usize..range.end as usize],
    )
}
fn batch_id(batch: &ObservationBatch) -> Result<String> {
    let legacy = hash(
        "forge-observation-batch-v1",
        &(
            &batch.source_session_id,
            batch.range,
            &batch.source_fingerprint,
            &batch.observer_version,
            &batch.created_at,
            batch
                .observations
                .iter()
                .map(|o| (o.scope, o.kind, &o.content))
                .collect::<Vec<_>>(),
        ),
    )?;
    match &batch.observer_job_id {
        Some(job_id) => hash("forge-observer-batch-v1", &(legacy, job_id)),
        None => Ok(legacy),
    }
}
impl ValidatedObservationBatch {
    pub(crate) fn bind_observer_job(mut self, job_id: &str) -> Result<Self> {
        self.batch.observer_job_id = Some(job_id.into());
        self.batch.id = batch_id(&self.batch)?;
        for (i, observation) in self.batch.observations.iter_mut().enumerate() {
            observation.id = hash("forge-observation-v1", &(&self.batch.id, i))?;
        }
        Ok(self)
    }
    pub fn new(
        source_session_id: &str,
        source_snapshot: &[Event],
        range: SourceRange,
        observer_version: &str,
        drafts: Vec<ObservationDraft>,
        redactor: &Redactor,
    ) -> Result<Self> {
        metadata(observer_version, redactor)?;
        let snapshot = snapshot(source_session_id, source_snapshot, redactor)?;
        if !range.valid(snapshot.hashes.len()) || range.start < snapshot.local_start {
            return Err(ObservationError::InvalidSource);
        }
        if drafts.len() > MAX_OBSERVATIONS_PER_BATCH {
            return Err(ObservationError::LimitExceeded);
        }
        let mut observations = Vec::with_capacity(drafts.len());
        for draft in drafts {
            if draft.content.len() > MAX_OBSERVATION_CONTENT_BYTES {
                return Err(ObservationError::LimitExceeded);
            }
            let content = redactor.redact(&draft.content);
            if content.trim().is_empty() {
                return Err(ObservationError::InvalidContent);
            }
            if content.len() > MAX_OBSERVATION_CONTENT_BYTES {
                return Err(ObservationError::LimitExceeded);
            }
            observations.push(Observation {
                id: String::new(),
                scope: draft.scope,
                kind: draft.kind,
                content,
            });
        }
        let mut batch = ObservationBatch {
            id: String::new(),
            source_session_id: source_session_id.into(),
            range,
            source_fingerprint: fingerprint(&snapshot, range)?,
            observer_version: observer_version.into(),
            created_at: source_snapshot[(range.end - 1) as usize].ts.to_rfc3339(),
            observations,
            observer_job_id: None,
        };
        batch.id = batch_id(&batch)?;
        for (i, observation) in batch.observations.iter_mut().enumerate() {
            observation.id = hash("forge-observation-v1", &(&batch.id, i))?;
        }
        Ok(Self { batch, snapshot })
    }
}

#[derive(Clone, Debug)]
pub struct LedgerProjection {
    session_id: String,
    batches: Vec<ObservationBatch>,
    tombstoned: BTreeSet<String>,
}
impl LedgerProjection {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
    pub fn batches(&self) -> &[ObservationBatch] {
        &self.batches
    }
    pub fn is_tombstoned(&self, observation_id: &str) -> bool {
        self.tombstoned.contains(observation_id)
    }
    pub fn tombstoned(&self) -> &BTreeSet<String> {
        &self.tombstoned
    }
    /// Empty batches count as processed intervals, not as rendered source coverage.
    pub fn covered_intervals(&self) -> Vec<SourceRange> {
        let mut ranges: Vec<_> = self.batches.iter().map(|b| b.range).collect();
        ranges.sort_by_key(|r| r.start);
        let mut merged: Vec<SourceRange> = Vec::new();
        for range in ranges {
            if let Some(last) = merged.last_mut()
                && last.end + 1 == range.start
            {
                last.end = range.end;
            } else {
                merged.push(range);
            }
        }
        merged
    }
    pub fn contiguous_watermark(&self) -> u64 {
        self.covered_intervals()
            .first()
            .filter(|r| r.start == 1)
            .map_or(0, |r| r.end)
    }
}
pub trait ObservationStore: Send + Sync {
    fn commit(&self, batch: ValidatedObservationBatch) -> Result<ObservationBatch>;
    /// True only for a durably initialized fork ledger, including frozen-empty.
    fn fork_initialized(&self, session_id: &str) -> Result<bool>;
    fn projection(
        &self,
        session_id: &str,
        source_snapshot: &[Event],
        redactor: &Redactor,
    ) -> Result<LedgerProjection>;
    fn tombstone(
        &self,
        session_id: &str,
        observation_ids: &BTreeSet<String>,
        source_snapshot: &[Event],
        redactor: &Redactor,
    ) -> Result<LedgerProjection>;
    #[allow(clippy::too_many_arguments)]
    fn fork(
        &self,
        parent_session: &str,
        parent_snapshot: &[Event],
        child_session: &str,
        child_snapshot: &[Event],
        cut: u64,
        redactor: &Redactor,
    ) -> Result<LedgerProjection>;
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    snapshot: Snapshot,
    batches: Vec<ObservationBatch>,
    #[serde(default)]
    tombstoned_batches: BTreeSet<String>,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    sessions: BTreeMap<String, Ledger>,
}
fn compatible(old: &Snapshot, new: &Snapshot) -> Result<()> {
    if old.local_start != new.local_start || !new.hashes.starts_with(&old.hashes) {
        return Err(ObservationError::SourceChanged);
    }
    Ok(())
}
fn validate(index: &Index) -> Result<()> {
    let mut count = 0;
    for (session, ledger) in &index.sessions {
        if !identifier(session)
            || ledger.snapshot.hashes.len() > MAX_OBSERVATION_SOURCE_EVENTS
            || ledger.snapshot.local_start == 0
            || ledger.snapshot.local_start > ledger.snapshot.hashes.len() as u64 + 1
            || ledger
                .snapshot
                .hashes
                .iter()
                .any(|h| h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(ObservationError::Corrupt);
        }
        for (i, b) in ledger.batches.iter().enumerate() {
            count += 1;
            if count > MAX_OBSERVATION_BATCHES
                || !identifier(&b.source_session_id)
                || !identifier(&b.observer_version)
                || b.created_at.len() > 64
                || b.observations.len() > MAX_OBSERVATIONS_PER_BATCH
                || b.observer_job_id
                    .as_ref()
                    .is_some_and(|id| id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()))
                || fingerprint(&ledger.snapshot, b.range)? != b.source_fingerprint
                || batch_id(b)? != b.id
                || ledger.batches[..i]
                    .iter()
                    .any(|prev| prev.range.overlaps(b.range))
            {
                return Err(ObservationError::Corrupt);
            }
            for (j, o) in b.observations.iter().enumerate() {
                if o.content.trim().is_empty()
                    || o.content.len() > MAX_OBSERVATION_CONTENT_BYTES
                    || o.id != hash("forge-observation-v1", &(&b.id, j))?
                {
                    return Err(ObservationError::Corrupt);
                }
            }
        }
        let known: BTreeSet<_> = ledger.batches.iter().map(|batch| &batch.id).collect();
        if ledger
            .tombstoned_batches
            .iter()
            .any(|batch_id| !known.contains(batch_id))
        {
            return Err(ObservationError::Corrupt);
        }
    }
    if index.sessions.len() > 1024 {
        return Err(ObservationError::LimitExceeded);
    }
    Ok(())
}
fn load(files: &dyn crate::artifact::ArtifactFiles) -> Result<Index> {
    let bytes = files
        .read("index.json", MAX_OBSERVATION_LEDGER_BYTES)
        .map_err(|_| ObservationError::Unavailable)?;
    let index = match bytes {
        Some(bytes) => serde_json::from_slice(&bytes).map_err(|_| ObservationError::Corrupt)?,
        None => Index::default(),
    };
    validate(&index)?;
    Ok(index)
}
fn save(files: &mut dyn crate::artifact::ArtifactFiles, index: &Index) -> Result<()> {
    validate(index)?;
    let bytes = serde_json::to_vec(index).map_err(|_| ObservationError::Corrupt)?;
    if bytes.len() > MAX_OBSERVATION_LEDGER_BYTES {
        return Err(ObservationError::LimitExceeded);
    }
    files
        .write("index.json", &bytes)
        .map_err(|_| ObservationError::Unavailable)
}
fn tombstoned_observation_ids(ledger: &Ledger) -> BTreeSet<String> {
    ledger
        .batches
        .iter()
        .filter(|batch| ledger.tombstoned_batches.contains(&batch.id))
        .flat_map(|batch| {
            batch
                .observations
                .iter()
                .map(|observation| observation.id.clone())
        })
        .collect()
}
#[derive(Default)]
pub struct MemoryObservationStore {
    files: Mutex<BTreeMap<String, Vec<u8>>>,
}
pub struct FsObservationStore {
    root: PathBuf,
}
impl FsObservationStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    /// Native read-only snapshot: no locks, repairs, creation or LRU writes.
    /// `None` means the ledger is absent, not corrupt.
    pub fn inspect(
        &self,
        session_id: &str,
        events: &[Event],
        redactor: &Redactor,
    ) -> Result<Option<LedgerProjection>> {
        let Some(bytes) =
            crate::store::namespace_read(&self.root, "observations", MAX_OBSERVATION_LEDGER_BYTES)
                .map_err(|_| ObservationError::Unavailable)?
        else {
            return Ok(None);
        };
        let index: Index = serde_json::from_slice(&bytes).map_err(|_| ObservationError::Corrupt)?;
        validate(&index)?;
        let source = snapshot(session_id, events, redactor)?;
        let batches = if let Some(ledger) = index.sessions.get(session_id) {
            compatible(&ledger.snapshot, &source)?;
            ledger.batches.clone()
        } else if source.local_start > 1 {
            return Err(ObservationError::InvalidFork);
        } else {
            Vec::new()
        };
        Ok(Some(LedgerProjection {
            session_id: session_id.into(),
            batches,
            tombstoned: index
                .sessions
                .get(session_id)
                .map_or_else(BTreeSet::new, tombstoned_observation_ids),
        }))
    }

    fn transaction<T>(
        &self,
        operation: &mut dyn FnMut(&mut dyn crate::artifact::ArtifactFiles) -> Result<T>,
    ) -> Result<T> {
        // Keep domain errors typed without embedding any native path/content.
        crate::store::observation_transaction(&self.root, &mut |files| Ok(operation(files)))
            .map_err(|_| ObservationError::Unavailable)?
    }
}
impl MemoryObservationStore {
    fn transaction<T>(
        &self,
        operation: &mut dyn FnMut(&mut dyn crate::artifact::ArtifactFiles) -> Result<T>,
    ) -> Result<T> {
        operation(
            &mut *self
                .files
                .lock()
                .map_err(|_| ObservationError::Unavailable)?,
        )
    }
}
macro_rules! impl_store {
    ($ty:ty) => {
        impl ObservationStore for $ty {
            fn fork_initialized(&self, session_id: &str) -> Result<bool> {
                if !identifier(session_id) { return Err(ObservationError::InvalidMetadata); }
                self.transaction(&mut |files| {
                    Ok(load(files)?.sessions.get(session_id).is_some_and(|ledger| ledger.snapshot.local_start > 1))
                })
            }
            fn commit(&self, batch: ValidatedObservationBatch) -> Result<ObservationBatch> {
                self.transaction(&mut |files| {
                    let mut index = load(files)?;
                    let session = &batch.batch.source_session_id;
                    if !index.sessions.contains_key(session) && batch.snapshot.local_start != 1 {
                        return Err(ObservationError::InvalidFork);
                    }
                    let ledger = index.sessions.entry(session.clone()).or_insert_with(|| Ledger {
                        snapshot: batch.snapshot.clone(), batches: Vec::new(), tombstoned_batches: BTreeSet::new(),
                    });
                    // Disjoint jobs may finish out of order with shorter snapshots.
                    if ledger.snapshot.hashes.len() <= batch.snapshot.hashes.len() {
                        compatible(&ledger.snapshot, &batch.snapshot)?;
                        ledger.snapshot = batch.snapshot.clone();
                    } else { compatible(&batch.snapshot, &ledger.snapshot)?; }
                    if ledger.batches.iter().any(|b| b.range.overlaps(batch.batch.range)) {
                        return Err(ObservationError::Overlap);
                    }
                    ledger.batches.push(batch.batch.clone());
                    save(files, &index)?;
                    Ok(batch.batch.clone())
                })
            }
            fn projection(&self, session_id: &str, source_snapshot: &[Event], redactor: &Redactor) -> Result<LedgerProjection> {
                let source = snapshot(session_id, source_snapshot, redactor)?;
                self.transaction(&mut |files| {
                    let index = load(files)?;
                    let batches = if let Some(ledger) = index.sessions.get(session_id) {
                        compatible(&ledger.snapshot, &source)?;
                        ledger.batches.clone()
                    } else { Vec::new() };
                    let tombstoned = index.sessions.get(session_id)
                        .map_or_else(BTreeSet::new, tombstoned_observation_ids);
                    Ok(LedgerProjection { session_id: session_id.into(), batches, tombstoned })
                })
            }
            fn tombstone(&self, session_id: &str, observation_ids: &BTreeSet<String>, source_snapshot: &[Event], redactor: &Redactor) -> Result<LedgerProjection> {
                if !identifier(session_id) { return Err(ObservationError::InvalidMetadata); }
                let source = snapshot(session_id, source_snapshot, redactor)?;
                self.transaction(&mut |files| {
                    let mut index = load(files)?;
                    let ledger = index.sessions.get_mut(session_id).ok_or(ObservationError::InvalidSource)?;
                    compatible(&ledger.snapshot, &source)?;
                    let known: BTreeSet<_> = ledger.batches.iter().flat_map(|batch| {
                        batch.observations.iter().map(|observation| observation.id.clone())
                    }).collect();
                    if observation_ids.iter().any(|id| !known.contains(id)) {
                        return Err(ObservationError::InvalidSource);
                    }
                    let selected_batches: BTreeSet<_> = ledger.batches.iter()
                        .filter(|batch| batch.observations.iter().any(|observation| observation_ids.contains(&observation.id)))
                        .map(|batch| batch.id.clone())
                        .collect();
                    let partial_batch = ledger.batches.iter()
                        .filter(|batch| selected_batches.contains(&batch.id))
                        .any(|batch| batch.observations.iter().any(|observation| !observation_ids.contains(&observation.id)));
                    if partial_batch {
                        return Err(ObservationError::InvalidSource);
                    }
                    ledger.tombstoned_batches.extend(selected_batches);
                    let batches = ledger.batches.clone();
                    let tombstoned = tombstoned_observation_ids(ledger);
                    save(files, &index)?;
                    Ok(LedgerProjection { session_id: session_id.into(), batches, tombstoned })
                })
            }
            fn fork(&self, parent_session: &str, parent_snapshot: &[Event], child_session: &str,
                child_snapshot: &[Event], cut: u64, redactor: &Redactor) -> Result<LedgerProjection> {
                let parent = snapshot(parent_session, parent_snapshot, redactor)?;
                let child = snapshot(child_session, child_snapshot, redactor)?;
                if parent_session == child_session || cut > parent.hashes.len() as u64
                    || child.hashes.len() <= cut as usize || child.local_start != cut + 2
                    || parent.hashes[..cut as usize] != child.hashes[..cut as usize]
                    || !matches!(&child_snapshot[cut as usize].kind, EventKind::SessionForked { from_session, at_position }
                        if from_session == parent_session && *at_position == cut) {
                    return Err(ObservationError::InvalidFork);
                }
                self.transaction(&mut |files| {
                    let mut index = load(files)?;
                    if index.sessions.contains_key(child_session) { return Err(ObservationError::InvalidFork); }
                    let batches = if let Some(ledger) = index.sessions.get(parent_session) {
                        compatible(&ledger.snapshot, &parent)?;
                        ledger.batches.iter().filter(|b| b.range.end <= cut).cloned().collect()
                    } else { Vec::new() };
                    // Consolidated topic files are session-local. A fork gets
                    // the source observations so it can consolidate its own
                    // durable view instead of inheriting hidden tombstones.
                    index.sessions.insert(child_session.into(), Ledger { snapshot: child.clone(), batches: batches.clone(), tombstoned_batches: BTreeSet::new() });
                    save(files, &index)?;
                    Ok(LedgerProjection { session_id: child_session.into(), batches, tombstoned: BTreeSet::new() })
                })
            }
        }
    };
}
impl_store!(MemoryObservationStore);
impl_store!(FsObservationStore);

#[cfg(test)]
mod tests;
