//! Durable, source-linked views over mature session observations.
//!
//! The observation ledger remains authoritative. Topic objects commit before
//! callers tombstone their source observations, so interruption can only leave
//! an unreferenced object or an idempotently resumable index entry.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

use crate::{
    LedgerProjection, ObservationKind, ObservationScope, artifact::ArtifactFiles, observation::hash,
};

pub const CONSOLIDATION_VERSION: u32 = 1;
const MAX_INDEX_BYTES: usize = 4 * 1024 * 1024;
const MAX_TOPIC_BYTES: usize = 4 * 1024 * 1024;
const MAX_CLAIMS: usize = 16_384;
const MAX_SEARCH_RESULTS: usize = 50;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsolidationError {
    Invalid,
    Corrupt,
    Unavailable,
    NotFound,
}
impl std::fmt::Display for ConsolidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Invalid => "invalid consolidation request",
            Self::Corrupt => "consolidated memory is corrupt",
            Self::Unavailable => "consolidated memory is unavailable",
            Self::NotFound => "consolidated topic not found",
        })
    }
}
impl std::error::Error for ConsolidationError {}
type Result<T> = std::result::Result<T, ConsolidationError>;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClaimSource {
    pub observation_id: String,
    pub batch_id: String,
    pub source_session_id: String,
    pub source_start: u64,
    pub source_end: u64,
    pub source_fingerprint: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TopicClaim {
    pub scope: ObservationScope,
    pub kind: ObservationKind,
    pub content: String,
    pub source: ClaimSource,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TopicFile {
    pub version: u32,
    pub session_id: String,
    pub topic: String,
    pub claims: Vec<TopicClaim>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct TopicRef {
    object: String,
    claims: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SessionIndex {
    topics: BTreeMap<String, TopicRef>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Index {
    version: u32,
    sessions: BTreeMap<String, SessionIndex>,
    /// Explicit promotions, keyed by `<source-session>:<topic>`.
    project_topics: BTreeMap<String, TopicRef>,
}
impl Default for Index {
    fn default() -> Self {
        Self {
            version: CONSOLIDATION_VERSION,
            sessions: BTreeMap::new(),
            project_topics: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ConsolidationReport {
    pub session_id: String,
    pub topics_written: usize,
    pub claims_written: usize,
    pub consumed_observation_ids: BTreeSet<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PromotionReport {
    pub session_id: String,
    pub topic: String,
    pub claims: usize,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct LexicalMatch {
    pub project_memory: bool,
    pub session_id: String,
    pub topic: String,
    pub observation_id: String,
    pub content: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ConsolidationStatus {
    pub session_topics: usize,
    pub session_claims: usize,
    pub project_topics: usize,
}

pub struct FsConsolidationStore {
    root: PathBuf,
}
impl FsConsolidationStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Read-only metadata inspection. Missing storage stays missing and does
    /// not create the context directory.
    pub fn status(&self, session: &str) -> Result<Option<ConsolidationStatus>> {
        if !crate::observation::identifier(session) {
            return Err(ConsolidationError::Invalid);
        }
        let Some(bytes) =
            crate::store::namespace_read(&self.root, "consolidation", MAX_INDEX_BYTES)
                .map_err(|_| ConsolidationError::Unavailable)?
        else {
            return Ok(None);
        };
        let index: Index =
            serde_json::from_slice(&bytes).map_err(|_| ConsolidationError::Corrupt)?;
        validate_index(&index)?;
        let entry = index.sessions.get(session);
        Ok(Some(ConsolidationStatus {
            session_topics: entry.map_or(0, |entry| entry.topics.len()),
            session_claims: entry.map_or(0, |entry| {
                entry
                    .topics
                    .values()
                    .map(|reference| reference.claims)
                    .sum()
            }),
            project_topics: index.project_topics.len(),
        }))
    }

    pub fn consolidate(&self, projection: &LedgerProjection) -> Result<ConsolidationReport> {
        let session = projection.session_id();
        if !crate::observation::identifier(session) {
            return Err(ConsolidationError::Invalid);
        }
        let mut grouped: BTreeMap<String, Vec<TopicClaim>> = BTreeMap::new();
        for batch in projection.batches() {
            for observation in &batch.observations {
                if projection.is_tombstoned(&observation.id) {
                    continue;
                }
                grouped
                    .entry(topic_name(observation.kind).into())
                    .or_default()
                    .push(TopicClaim {
                        scope: observation.scope,
                        kind: observation.kind,
                        content: observation.content.clone(),
                        source: ClaimSource {
                            observation_id: observation.id.clone(),
                            batch_id: batch.id.clone(),
                            source_session_id: batch.source_session_id.clone(),
                            source_start: batch.range.start,
                            source_end: batch.range.end,
                            source_fingerprint: batch.source_fingerprint.clone(),
                        },
                    });
            }
        }
        self.transaction(&mut |files| {
            let mut index = load_index(files)?;
            let existing = index.sessions.entry(session.into()).or_default().clone();
            let mut next = existing;
            let mut consumed = BTreeSet::new();
            let mut written = 0;
            for (topic, mut claims) in grouped.clone() {
                if let Some(reference) = next.topics.get(&topic) {
                    let old = load_topic(files, reference)?;
                    if old.session_id != session || old.topic != topic {
                        return Err(ConsolidationError::Corrupt);
                    }
                    claims.extend(old.claims);
                }
                claims.sort_by(|a, b| a.source.observation_id.cmp(&b.source.observation_id));
                claims.dedup_by(|a, b| a.source.observation_id == b.source.observation_id);
                if claims.len() > MAX_CLAIMS {
                    return Err(ConsolidationError::Invalid);
                }
                consumed.extend(
                    claims
                        .iter()
                        .map(|claim| claim.source.observation_id.clone()),
                );
                let topic_file = TopicFile {
                    version: CONSOLIDATION_VERSION,
                    session_id: session.into(),
                    topic: topic.clone(),
                    claims,
                };
                let bytes =
                    serde_json::to_vec(&topic_file).map_err(|_| ConsolidationError::Corrupt)?;
                if bytes.len() > MAX_TOPIC_BYTES {
                    return Err(ConsolidationError::Invalid);
                }
                let object = hash("forge-consolidated-topic-v1", &topic_file)
                    .map_err(|_| ConsolidationError::Corrupt)?;
                files
                    .write(&object, &bytes)
                    .map_err(|_| ConsolidationError::Unavailable)?;
                next.topics.insert(
                    topic,
                    TopicRef {
                        object,
                        claims: topic_file.claims.len(),
                    },
                );
                written += 1;
            }
            index.sessions.insert(session.into(), next);
            save_index(files, &index)?;
            Ok(ConsolidationReport {
                session_id: session.into(),
                topics_written: written,
                claims_written: consumed.len(),
                consumed_observation_ids: consumed,
            })
        })
    }

    pub fn promote(&self, session: &str, topic: &str) -> Result<PromotionReport> {
        if !crate::observation::identifier(session) || !valid_topic(topic) {
            return Err(ConsolidationError::Invalid);
        }
        self.transaction(&mut |files| {
            let mut index = load_index(files)?;
            let reference = index
                .sessions
                .get(session)
                .and_then(|entry| entry.topics.get(topic))
                .cloned()
                .ok_or(ConsolidationError::NotFound)?;
            let file = load_topic(files, &reference)?;
            if file.session_id != session || file.topic != topic {
                return Err(ConsolidationError::Corrupt);
            }
            index
                .project_topics
                .insert(format!("{session}:{topic}"), reference.clone());
            save_index(files, &index)?;
            Ok(PromotionReport {
                session_id: session.into(),
                topic: topic.into(),
                claims: reference.claims,
            })
        })
    }

    pub fn search(&self, session: &str, query: &str) -> Result<Vec<LexicalMatch>> {
        if !crate::observation::identifier(session) || query.trim().is_empty() || query.len() > 256
        {
            return Err(ConsolidationError::Invalid);
        }
        let needle = query.to_lowercase();
        self.transaction(&mut |files| {
            let index = load_index(files)?;
            let mut selected = Vec::new();
            if let Some(entry) = index.sessions.get(session) {
                selected.extend(entry.topics.iter().map(|(topic, reference)| {
                    (false, session.to_owned(), topic.clone(), reference.clone())
                }));
            }
            selected.extend(index.project_topics.iter().filter_map(|(key, reference)| {
                let (source_session, topic) = key.split_once(':')?;
                Some((true, source_session.into(), topic.into(), reference.clone()))
            }));
            let mut matches = Vec::new();
            for (project_memory, source_session, topic, reference) in selected {
                let file = load_topic(files, &reference)?;
                if file.session_id != source_session || file.topic != topic {
                    return Err(ConsolidationError::Corrupt);
                }
                for claim in file.claims {
                    if topic.to_lowercase().contains(&needle)
                        || claim.content.to_lowercase().contains(&needle)
                    {
                        matches.push(LexicalMatch {
                            project_memory,
                            session_id: source_session.clone(),
                            topic: topic.clone(),
                            observation_id: claim.source.observation_id,
                            content: claim.content,
                        });
                        if matches.len() == MAX_SEARCH_RESULTS {
                            return Ok(matches);
                        }
                    }
                }
            }
            Ok(matches)
        })
    }

    fn transaction<T>(
        &self,
        operation: &mut dyn FnMut(&mut dyn ArtifactFiles) -> Result<T>,
    ) -> Result<T> {
        crate::store::consolidation_transaction(&self.root, &mut |files| Ok(operation(files)))
            .map_err(|_| ConsolidationError::Unavailable)?
    }
}

fn topic_name(kind: ObservationKind) -> &'static str {
    match kind {
        ObservationKind::Decision => "decisions",
        ObservationKind::Constraint => "constraints",
        ObservationKind::Outcome => "outcomes",
        ObservationKind::Question => "questions",
        ObservationKind::State => "state",
    }
}
fn valid_topic(topic: &str) -> bool {
    matches!(
        topic,
        "decisions" | "constraints" | "outcomes" | "questions" | "state"
    )
}
fn load_index(files: &dyn ArtifactFiles) -> Result<Index> {
    let index = match files
        .read("index.json", MAX_INDEX_BYTES)
        .map_err(|_| ConsolidationError::Unavailable)?
    {
        Some(bytes) => serde_json::from_slice(&bytes).map_err(|_| ConsolidationError::Corrupt)?,
        None => Index::default(),
    };
    if index.version != CONSOLIDATION_VERSION {
        return Err(ConsolidationError::Corrupt);
    }
    validate_index(&index)?;
    Ok(index)
}
fn save_index(files: &mut dyn ArtifactFiles, index: &Index) -> Result<()> {
    validate_index(index)?;
    let bytes = serde_json::to_vec(index).map_err(|_| ConsolidationError::Corrupt)?;
    if bytes.len() > MAX_INDEX_BYTES {
        return Err(ConsolidationError::Invalid);
    }
    files
        .write("index.json", &bytes)
        .map_err(|_| ConsolidationError::Unavailable)
}
fn load_topic(files: &dyn ArtifactFiles, reference: &TopicRef) -> Result<TopicFile> {
    let bytes = files
        .read(&reference.object, MAX_TOPIC_BYTES)
        .map_err(|_| ConsolidationError::Unavailable)?
        .ok_or(ConsolidationError::Corrupt)?;
    let topic: TopicFile =
        serde_json::from_slice(&bytes).map_err(|_| ConsolidationError::Corrupt)?;
    if topic.version != CONSOLIDATION_VERSION
        || topic.claims.len() != reference.claims
        || !crate::observation::identifier(&topic.session_id)
        || !valid_topic(&topic.topic)
        || topic.claims.iter().any(|claim| {
            topic_name(claim.kind) != topic.topic
                || !valid_hash(&claim.source.observation_id)
                || !valid_hash(&claim.source.batch_id)
                || !crate::observation::identifier(&claim.source.source_session_id)
                || claim.source.source_start == 0
                || claim.source.source_start > claim.source.source_end
                || !valid_hash(&claim.source.source_fingerprint)
                || claim.content.trim().is_empty()
                || claim.content.len() > crate::MAX_OBSERVATION_CONTENT_BYTES
        })
        || hash("forge-consolidated-topic-v1", &topic).map_err(|_| ConsolidationError::Corrupt)?
            != reference.object
    {
        return Err(ConsolidationError::Corrupt);
    }
    Ok(topic)
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_index(index: &Index) -> Result<()> {
    if index.version != CONSOLIDATION_VERSION || index.sessions.len() > 1024 {
        return Err(ConsolidationError::Corrupt);
    }
    let valid_ref = |reference: &TopicRef| {
        valid_hash(&reference.object) && reference.claims > 0 && reference.claims <= MAX_CLAIMS
    };
    for (session, entry) in &index.sessions {
        if !crate::observation::identifier(session)
            || entry.topics.len() > 5
            || entry
                .topics
                .iter()
                .any(|(topic, reference)| !valid_topic(topic) || !valid_ref(reference))
        {
            return Err(ConsolidationError::Corrupt);
        }
    }
    for (key, reference) in &index.project_topics {
        let Some((session, topic)) = key.split_once(':') else {
            return Err(ConsolidationError::Corrupt);
        };
        if !crate::observation::identifier(session) || !valid_topic(topic) || !valid_ref(reference)
        {
            return Err(ConsolidationError::Corrupt);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        MemoryObservationStore, ObservationDraft, ObservationStore, SourceRange,
        ValidatedObservationBatch,
    };
    use forge_core::{Event, EventKind};
    use forge_session::Redactor;

    fn events(session: &str) -> Vec<Event> {
        (0..4)
            .map(|n| {
                let mut event = Event::new(
                    "run",
                    session,
                    EventKind::InputReceived {
                        message: format!("source {n}"),
                    },
                );
                event.ts = "2026-10-09T00:00:00Z".parse().unwrap();
                event
            })
            .collect()
    }

    #[test]
    fn consolidation_is_idempotent_source_linked_and_explicitly_promoted() {
        let source = events("session-a");
        let redactor = Redactor::default();
        let observations = MemoryObservationStore::default();
        observations
            .commit(
                ValidatedObservationBatch::new(
                    "session-a",
                    &source,
                    SourceRange { start: 1, end: 2 },
                    "observer-v1",
                    vec![
                        ObservationDraft {
                            scope: ObservationScope::Session,
                            kind: ObservationKind::Decision,
                            content: "choose the bounded lexical index".into(),
                        },
                        ObservationDraft {
                            scope: ObservationScope::Session,
                            kind: ObservationKind::Constraint,
                            content: "retain every source identifier".into(),
                        },
                    ],
                    &redactor,
                )
                .unwrap(),
            )
            .unwrap();
        let projection = observations
            .projection("session-a", &source, &redactor)
            .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let store = FsConsolidationStore::new(tmp.path());

        let first = store.consolidate(&projection).unwrap();
        let resumed = store.consolidate(&projection).unwrap();
        assert_eq!(
            first.consumed_observation_ids,
            resumed.consumed_observation_ids
        );
        assert_eq!(first.claims_written, 2);
        let local = store.search("session-a", "lexical").unwrap();
        assert_eq!(local.len(), 1);
        assert!(!local[0].project_memory);
        assert!(
            first
                .consumed_observation_ids
                .contains(&local[0].observation_id)
        );

        // An object committed before an interrupted index publication is an
        // invisible orphan. Re-running deterministically reuses/replaces that
        // object and publishes the complete index.
        let object = std::fs::read_dir(tmp.path().join("consolidation/objects"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let interrupted = tempfile::tempdir().unwrap();
        let interrupted_objects = interrupted.path().join("consolidation/objects");
        std::fs::create_dir_all(&interrupted_objects).unwrap();
        std::fs::copy(
            &object,
            interrupted_objects.join(object.file_name().unwrap()),
        )
        .unwrap();
        let recovered = FsConsolidationStore::new(interrupted.path());
        assert_eq!(
            recovered.consolidate(&projection).unwrap().claims_written,
            2
        );
        assert_eq!(recovered.search("session-a", "lexical").unwrap().len(), 1);

        assert!(store.search("session-b", "lexical").unwrap().is_empty());
        let promoted = store.promote("session-a", "decisions").unwrap();
        assert_eq!(promoted.claims, 1);
        let project = store.search("session-b", "lexical").unwrap();
        assert_eq!(project.len(), 1);
        assert!(project[0].project_memory);

        let tombstoned = observations
            .tombstone(
                "session-a",
                &first.consumed_observation_ids,
                &source,
                &redactor,
            )
            .unwrap();
        assert_eq!(tombstoned.tombstoned(), &first.consumed_observation_ids);
        assert!(
            tombstoned
                .batches()
                .iter()
                .flat_map(|batch| &batch.observations)
                .all(|observation| tombstoned.is_tombstoned(&observation.id))
        );
    }

    #[test]
    fn invalid_topics_queries_and_missing_promotions_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FsConsolidationStore::new(tmp.path());
        assert_eq!(
            store.promote("session-a", "../../guidance").unwrap_err(),
            ConsolidationError::Invalid
        );
        assert_eq!(
            store.promote("session-a", "decisions").unwrap_err(),
            ConsolidationError::NotFound
        );
        assert_eq!(
            store.search("session-a", " ").unwrap_err(),
            ConsolidationError::Invalid
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_storage_is_private_and_never_follows_links() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let source = events("session-a");
        let redactor = Redactor::default();
        let observations = MemoryObservationStore::default();
        observations
            .commit(
                ValidatedObservationBatch::new(
                    "session-a",
                    &source,
                    SourceRange { start: 1, end: 1 },
                    "observer-v1",
                    vec![ObservationDraft {
                        scope: ObservationScope::Session,
                        kind: ObservationKind::State,
                        content: "private durable state".into(),
                    }],
                    &redactor,
                )
                .unwrap(),
            )
            .unwrap();
        let projection = observations
            .projection("session-a", &source, &redactor)
            .unwrap();

        let linked = tempfile::tempdir().unwrap();
        let victim = tempfile::tempdir().unwrap();
        symlink(victim.path(), linked.path().join("consolidation")).unwrap();
        assert!(
            FsConsolidationStore::new(linked.path())
                .consolidate(&projection)
                .is_err()
        );
        assert_eq!(std::fs::read_dir(victim.path()).unwrap().count(), 0);

        let root = tempfile::tempdir().unwrap();
        FsConsolidationStore::new(root.path())
            .consolidate(&projection)
            .unwrap();
        for path in ["consolidation/index.json", "consolidation/lock"] {
            assert_eq!(
                std::fs::metadata(root.path().join(path))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert_eq!(
            std::fs::metadata(root.path().join("consolidation"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let index = root.path().join("consolidation/index.json");
        let alias = root.path().join("alias");
        std::fs::hard_link(&index, &alias).unwrap();
        let before = std::fs::read(&alias).unwrap();
        assert!(
            FsConsolidationStore::new(root.path())
                .status("session-a")
                .is_err()
        );
        assert_eq!(std::fs::read(alias).unwrap(), before);
    }
}
