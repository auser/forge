//! Sanitized, source-authorized tool artifacts. The filesystem index is a
//! bounded transaction journal as well as the manifest table: object intents
//! precede object writes, and retired entries survive until deletion succeeds.
//! This avoids untracked crash garbage without unsafe directory enumeration.
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use forge_core::ForgeError;
use forge_session::Redactor;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const REDACTION_POLICY_VERSION: u32 = 1;
pub const MAX_ARTIFACT_MANIFESTS: usize = 4096;
pub const MAX_ARTIFACT_OBJECTS: usize = 4096;
pub const MAX_ARTIFACT_RESPONSE_BYTES: usize = 16 * 1024;
const MAX_INDEX_BYTES: usize = 8 * 1024 * 1024;
const INDEX: &str = "index.json";

/// Only the shared session redactor can construct persisted output. No
/// deserializer or unchecked constructor can bypass this boundary.
pub struct SanitizedOutput(String);

impl SanitizedOutput {
    pub fn new(redactor: &Redactor, raw: &str) -> Self {
        Self(redactor.redact(raw))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArtifactLimits {
    pub max_project_bytes: u64,
    pub max_age_secs: u64,
    pub max_artifact_bytes: u64,
}

impl Default for ArtifactLimits {
    fn default() -> Self {
        Self {
            max_project_bytes: 128 * 1024 * 1024,
            max_age_secs: 7 * 24 * 60 * 60,
            max_artifact_bytes: 8 * 1024 * 1024,
        }
    }
}

/// `event_seq` anchors the already-persisted ToolCallRequested event, not the
/// later result event. Authorization is supplied by the runtime's typed,
/// visible event history, including its exact fork cuts.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactSource {
    pub session_id: String,
    pub run_id: String,
    pub call_id: String,
    pub event_seq: u64,
}

/// Source metadata checked against the runtime/session's existing redactor.
/// Identifiers must remain unchanged so persisted event authorization and
/// artifact identity agree. No deserializer or unchecked constructor can
/// bypass this boundary.
pub struct SanitizedArtifactSource(ArtifactSource);

impl SanitizedArtifactSource {
    pub fn new(source: ArtifactSource, redactor: &Redactor) -> Result<Self, ForgeError> {
        if !valid_source(&source)
            || [&source.session_id, &source.run_id, &source.call_id]
                .iter()
                .any(|id| redactor.redact(id) != **id)
        {
            return Err(unavailable());
        }
        Ok(Self(source))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactRef {
    pub handle: String,
    pub source: ArtifactSource,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ArtifactQuery {
    /// Half-open byte range; at most 16 KiB requested. Boundaries move inward
    /// to UTF-8 boundaries. JSON escaping can shorten the returned window.
    Range { start: usize, end: usize },
    /// Find the first literal occurrence at or after `start`, returning a
    /// window of at most `limit` bytes starting there. Continue at next_start.
    Search {
        literal: String,
        start: usize,
        limit: usize,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArtifactRead {
    pub text: String,
    pub start: usize,
    pub end: usize,
    pub total_bytes: usize,
    pub next_start: Option<usize>,
}

pub trait ArtifactStore: Send + Sync {
    fn put(
        &self,
        source: SanitizedArtifactSource,
        output: &SanitizedOutput,
    ) -> Result<ArtifactRef, ForgeError>;

    /// Never authorize by content hash, model text, or session ID alone.
    /// `allowed` must come from trusted typed visible event metadata.
    /// Unauthorized, missing and expired artifacts all return `Ok(None)`.
    fn retrieve(
        &self,
        handle: &str,
        allowed: &[ArtifactRef],
        query: ArtifactQuery,
    ) -> Result<Option<ArtifactRead>, ForgeError>;
}

/// Epoch seconds; injectable to make age and LRU behavior deterministic.
pub trait ArtifactClock: Send + Sync {
    fn now_secs(&self) -> u64;
}

struct SystemClock;
impl ArtifactClock for SystemClock {
    fn now_secs(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }
}

pub struct FsArtifactStore {
    root: PathBuf,
    limits: ArtifactLimits,
    clock: Arc<dyn ArtifactClock>,
}

impl FsArtifactStore {
    /// Same `.forge/context` root convention as FsContextStore.
    pub fn new(root: impl Into<PathBuf>, limits: ArtifactLimits) -> Self {
        Self::with_clock(root, limits, Arc::new(SystemClock))
    }

    pub fn with_clock(
        root: impl Into<PathBuf>,
        limits: ArtifactLimits,
        clock: Arc<dyn ArtifactClock>,
    ) -> Self {
        Self {
            root: root.into(),
            limits,
            clock,
        }
    }
}

pub struct MemoryArtifactStore {
    files: Mutex<BTreeMap<String, Vec<u8>>>,
    limits: ArtifactLimits,
    clock: Arc<dyn ArtifactClock>,
}

impl Default for MemoryArtifactStore {
    fn default() -> Self {
        Self::new(ArtifactLimits::default())
    }
}

impl MemoryArtifactStore {
    pub fn new(limits: ArtifactLimits) -> Self {
        Self::with_clock(limits, Arc::new(SystemClock))
    }

    pub fn with_clock(limits: ArtifactLimits, clock: Arc<dyn ArtifactClock>) -> Self {
        Self {
            files: Mutex::new(BTreeMap::new()),
            limits,
            clock,
        }
    }
}

/// Names are exclusively INDEX or validated SHA-256 hex identities. Backends
/// pin both index and objects directories and never resolve caller paths.
pub(crate) trait ArtifactFiles {
    fn read(&self, name: &str, bound: usize) -> io::Result<Option<Vec<u8>>>;
    fn write(&mut self, name: &str, bytes: &[u8]) -> io::Result<()>;
    fn remove(&mut self, name: &str) -> io::Result<()>;
}

impl ArtifactFiles for BTreeMap<String, Vec<u8>> {
    fn read(&self, name: &str, bound: usize) -> io::Result<Option<Vec<u8>>> {
        match self.get(name) {
            Some(bytes) if bytes.len() > bound => Err(io::ErrorKind::InvalidData.into()),
            bytes => Ok(bytes.cloned()),
        }
    }
    fn write(&mut self, name: &str, bytes: &[u8]) -> io::Result<()> {
        self.insert(name.to_owned(), bytes.to_vec());
        Ok(())
    }
    fn remove(&mut self, name: &str) -> io::Result<()> {
        BTreeMap::remove(self, name);
        Ok(())
    }
}

fn unavailable() -> ForgeError {
    ForgeError::session("tool artifact storage unavailable")
}

macro_rules! implement_store {
    ($store:ty, $transaction:expr) => {
        impl ArtifactStore for $store {
            fn put(
                &self,
                source: SanitizedArtifactSource,
                output: &SanitizedOutput,
            ) -> Result<ArtifactRef, ForgeError> {
                let source = source.0;
                validate_put(&source, output, self.limits).map_err(|_| unavailable())?;
                ($transaction)(self, &mut |files: &mut dyn ArtifactFiles| {
                    put(
                        files,
                        source.clone(),
                        output,
                        self.limits,
                        self.clock.now_secs(),
                    )
                })
                .map_err(|_| unavailable())
            }
            fn retrieve(
                &self,
                handle: &str,
                allowed: &[ArtifactRef],
                query: ArtifactQuery,
            ) -> Result<Option<ArtifactRead>, ForgeError> {
                // Deny before touching storage or reporting query errors.
                if !valid_handle(handle) || !allowed.iter().any(|entry| entry.handle == handle) {
                    return Ok(None);
                }
                validate_query(&query)
                    .map_err(|_| ForgeError::session("invalid tool artifact query"))?;
                ($transaction)(self, &mut |files: &mut dyn ArtifactFiles| {
                    retrieve(
                        files,
                        handle,
                        allowed,
                        &query,
                        self.limits,
                        self.clock.now_secs(),
                    )
                })
                .map_err(|_| unavailable())
            }
        }
    };
}

impl FsArtifactStore {
    fn transaction<T>(
        &self,
        operation: &mut dyn FnMut(&mut dyn ArtifactFiles) -> io::Result<T>,
    ) -> io::Result<T> {
        crate::store::artifact_transaction(&self.root, operation)
    }

    /// Ledger metadata only: no payload verification, eviction or LRU refresh.
    pub fn inspect(&self) -> io::Result<Option<ArtifactMetadata>> {
        let Some(bytes) = crate::store::namespace_read(&self.root, "artifacts", MAX_INDEX_BYTES)?
        else {
            return Ok(None);
        };
        let files = BTreeMap::from([(INDEX.to_string(), bytes)]);
        let index = load(&files)?;
        let now = self.clock.now_secs();
        Ok(Some(ArtifactMetadata {
            ledgered_objects: index.objects.len(),
            ledgered_manifests: index.manifests.len(),
            ledgered_bytes: payload(&index),
            expired_manifests: index
                .manifests
                .values()
                .filter(|m| now.saturating_sub(m.created) >= self.limits.max_age_secs)
                .count(),
            max_project_bytes: self.limits.max_project_bytes,
            payload_integrity_verified: false,
        }))
    }
}
impl MemoryArtifactStore {
    fn transaction<T>(
        &self,
        operation: &mut dyn FnMut(&mut dyn ArtifactFiles) -> io::Result<T>,
    ) -> io::Result<T> {
        operation(&mut *self.files.lock().map_err(|_| io::ErrorKind::Other)?)
    }
}
implement_store!(FsArtifactStore, FsArtifactStore::transaction);
implement_store!(MemoryArtifactStore, MemoryArtifactStore::transaction);

#[derive(Debug, Clone, Serialize)]
pub struct ArtifactMetadata {
    pub ledgered_objects: usize,
    pub ledgered_manifests: usize,
    pub ledgered_bytes: u64,
    pub expired_manifests: usize,
    pub max_project_bytes: u64,
    pub payload_integrity_verified: bool,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    objects: BTreeMap<String, Object>,
    manifests: BTreeMap<String, Manifest>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Object {
    bytes: u64,
    policy: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    source: ArtifactSource,
    object: String,
    policy: u32,
    bytes: u64,
    created: u64,
    accessed: u64,
}

fn valid_handle(value: &str) -> bool {
    value.len() == 26 && ulid::Ulid::from_string(value).is_ok()
}

fn valid_source(source: &ArtifactSource) -> bool {
    [&source.session_id, &source.run_id, &source.call_id]
        .iter()
        .all(|id| {
            !id.is_empty()
                && id.len() <= 256
                && id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.:".contains(&c))
        })
}

pub(crate) fn valid_artifact_filename(name: &str) -> bool {
    name == INDEX
        || (name.len() == 64
            && name
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)))
}

fn identity(text: &str) -> String {
    identity_for_policy(text, REDACTION_POLICY_VERSION)
}

fn identity_for_policy(text: &str, policy: u32) -> String {
    let mut digest = Sha256::new();
    digest.update(b"forge-sanitized-tool-artifact\0");
    digest.update(policy.to_be_bytes());
    digest.update(text.as_bytes());
    format!("{:x}", digest.finalize())
}

fn validate_put(
    source: &ArtifactSource,
    output: &SanitizedOutput,
    limits: ArtifactLimits,
) -> io::Result<()> {
    if !valid_source(source)
        || limits.max_age_secs == 0
        || limits.max_artifact_bytes == 0
        || output.0.len() as u64 > limits.max_artifact_bytes
        || output.0.len() as u64 > limits.max_project_bytes
        || limits.max_project_bytes == 0
    {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    Ok(())
}

fn load(files: &dyn ArtifactFiles) -> io::Result<Index> {
    let Some(bytes) = files.read(INDEX, MAX_INDEX_BYTES)? else {
        return Ok(Index::default());
    };
    let index: Index = serde_json::from_slice(&bytes).map_err(|_| io::ErrorKind::InvalidData)?;
    if index.manifests.len() > MAX_ARTIFACT_MANIFESTS
        || index.objects.len() > MAX_ARTIFACT_OBJECTS
        || index.objects.iter().any(|(hash, obj)| {
            hash == INDEX
                || !valid_artifact_filename(hash)
                || obj.policy != REDACTION_POLICY_VERSION
        })
        || index.manifests.iter().any(|(handle, manifest)| {
            !valid_handle(handle)
                || !valid_source(&manifest.source)
                || manifest.policy != REDACTION_POLICY_VERSION
                || index
                    .objects
                    .get(&manifest.object)
                    .is_none_or(|object| object.bytes != manifest.bytes)
        })
    {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(index)
}

fn save(files: &mut dyn ArtifactFiles, index: &Index) -> io::Result<()> {
    let bytes = serde_json::to_vec(index).map_err(|_| io::ErrorKind::InvalidData)?;
    if bytes.len() > MAX_INDEX_BYTES {
        return Err(io::ErrorKind::InvalidData.into());
    }
    files.write(INDEX, &bytes)
}

/// Delete only ledgered, no-longer-referenced objects. Persist deletions after
/// success; a crash replays an idempotent deletion, never loses the journal.
fn collect(files: &mut dyn ArtifactFiles, index: &mut Index) -> io::Result<()> {
    let live: BTreeSet<_> = index.manifests.values().map(|m| m.object.clone()).collect();
    let garbage: Vec<_> = index
        .objects
        .keys()
        .filter(|hash| !live.contains(*hash))
        .cloned()
        .collect();
    if !garbage.is_empty() {
        for hash in garbage {
            files.remove(&hash)?;
            index.objects.remove(&hash);
        }
        save(files, index)?;
    }
    Ok(())
}

fn payload(index: &Index) -> u64 {
    index
        .objects
        .values()
        .fold(0u64, |sum, obj| sum.saturating_add(obj.bytes))
}

fn evict_one(index: &mut Index) {
    // Source manifests, not deduped hashes, carry LRU. Equal timestamps have a
    // total deterministic order (creation time, then opaque handle bytes).
    if let Some(handle) = index
        .manifests
        .iter()
        .min_by_key(|(handle, m)| (m.accessed, m.created, *handle))
        .map(|(handle, _)| handle.clone())
    {
        index.manifests.remove(&handle);
    }
}

fn prune(
    files: &mut dyn ArtifactFiles,
    index: &mut Index,
    limits: ArtifactLimits,
    now: u64,
) -> io::Result<()> {
    let before = index.manifests.len();
    index.manifests.retain(|_, manifest| {
        now.saturating_sub(manifest.created) < limits.max_age_secs
            && manifest.bytes <= limits.max_artifact_bytes
    });
    // First commit lost authorization, then delete bytes. Reads never observe
    // a committed live manifest referencing a partially replaced object.
    if index.manifests.len() != before {
        save(files, index)?;
    }
    collect(files, index)?;
    while payload(index) > limits.max_project_bytes {
        evict_one(index);
        save(files, index)?;
        collect(files, index)?;
    }
    Ok(())
}

fn put(
    files: &mut dyn ArtifactFiles,
    source: ArtifactSource,
    output: &SanitizedOutput,
    limits: ArtifactLimits,
    now: u64,
) -> io::Result<ArtifactRef> {
    let mut index = load(files)?;
    prune(files, &mut index, limits, now)?;
    let hash = identity(output.as_str());
    loop {
        let additional = if index.objects.contains_key(&hash) {
            0
        } else {
            output.0.len() as u64
        };
        if payload(&index).saturating_add(additional) <= limits.max_project_bytes
            && index.manifests.len() < MAX_ARTIFACT_MANIFESTS
            && (index.objects.contains_key(&hash) || index.objects.len() < MAX_ARTIFACT_OBJECTS)
        {
            break;
        }
        evict_one(&mut index);
        save(files, &index)?;
        collect(files, &mut index)?;
    }
    if !index.objects.contains_key(&hash) {
        index.objects.insert(
            hash.clone(),
            Object {
                bytes: output.0.len() as u64,
                policy: REDACTION_POLICY_VERSION,
            },
        );
        // Durable intent before bytes: an interrupted write remains collectible.
        save(files, &index)?;
        files.write(&hash, output.0.as_bytes())?;
    } else {
        // A lost or externally damaged object must not yield a successful new
        // reference. Only exact, bounded sanitized bytes are deduplicated.
        if files.read(&hash, output.0.len())?.as_deref() != Some(output.0.as_bytes()) {
            return Err(io::ErrorKind::InvalidData.into());
        }
    }
    let reference = ArtifactRef {
        handle: ulid::Ulid::new().to_string(),
        source,
    };
    index.manifests.insert(
        reference.handle.clone(),
        Manifest {
            source: reference.source.clone(),
            object: hash,
            policy: REDACTION_POLICY_VERSION,
            bytes: output.0.len() as u64,
            created: now,
            accessed: now,
        },
    );
    save(files, &index)?;
    Ok(reference)
}

fn validate_query(query: &ArtifactQuery) -> io::Result<()> {
    let valid = match query {
        ArtifactQuery::Range { start, end } => {
            end > start && end - start <= MAX_ARTIFACT_RESPONSE_BYTES
        }
        ArtifactQuery::Search { literal, limit, .. } => {
            !literal.is_empty()
                && literal.len() <= MAX_ARTIFACT_RESPONSE_BYTES
                && *limit > 0
                && *limit <= MAX_ARTIFACT_RESPONSE_BYTES
        }
    };
    if valid {
        Ok(())
    } else {
        Err(io::ErrorKind::InvalidInput.into())
    }
}

fn retrieve(
    files: &mut dyn ArtifactFiles,
    handle: &str,
    allowed: &[ArtifactRef],
    query: &ArtifactQuery,
    limits: ArtifactLimits,
    now: u64,
) -> io::Result<Option<ArtifactRead>> {
    let mut index = load(files)?;
    // Exact source comparison is required in addition to handle membership.
    if !index.manifests.get(handle).is_some_and(|m| {
        allowed
            .iter()
            .any(|r| r.handle == handle && r.source == m.source)
    }) {
        return Ok(None);
    }
    prune(files, &mut index, limits, now)?;
    let Some(manifest) = index.manifests.get_mut(handle) else {
        return Ok(None);
    };
    let bound = usize::try_from(manifest.bytes).map_err(|_| io::ErrorKind::InvalidData)?;
    let Some(bytes) = files.read(&manifest.object, bound)? else {
        return Ok(None);
    };
    if bytes.len() != bound {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let text = String::from_utf8(bytes).map_err(|_| io::ErrorKind::InvalidData)?;
    if identity(&text) != manifest.object {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let response = window(&text, query);
    manifest.accessed = manifest.accessed.max(now);
    save(files, &index)?;
    Ok(Some(response))
}

fn ceil_boundary(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset += 1;
    }
    offset
}

fn floor_boundary(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn window(text: &str, query: &ArtifactQuery) -> ArtifactRead {
    let (start, end) = match query {
        ArtifactQuery::Range { start, end } => {
            (ceil_boundary(text, *start), floor_boundary(text, *end))
        }
        ArtifactQuery::Search {
            literal,
            start,
            limit,
        } => {
            let from = ceil_boundary(text, *start);
            let found = text[from..]
                .find(literal)
                .map_or(text.len(), |offset| from + offset);
            (found, floor_boundary(text, found.saturating_add(*limit)))
        }
    };
    let mut read = ArtifactRead {
        text: String::new(),
        start,
        end: end.max(start),
        total_bytes: text.len(),
        next_start: None,
    };
    // Reserve 512 bytes for the runtime's enclosing response metadata.
    // Count JSON escaping, not merely UTF-8 content bytes.
    loop {
        read.text = text[read.start..read.end].to_owned();
        read.next_start = (read.end < text.len()).then(|| {
            // A query narrower than one scalar cannot return that scalar;
            // explicitly advance rather than suggesting an infinite retry.
            if read.start == read.end {
                ceil_boundary(text, read.end + 1)
            } else {
                read.end
            }
        });
        if serde_json::to_vec(&read)
            .expect("ArtifactRead is serializable")
            .len()
            <= MAX_ARTIFACT_RESPONSE_BYTES - 512
        {
            return read;
        }
        let excess = serde_json::to_vec(&read)
            .expect("ArtifactRead is serializable")
            .len()
            - (MAX_ARTIFACT_RESPONSE_BYTES - 512);
        read.end = floor_boundary(
            text,
            read.end.saturating_sub(excess.div_ceil(6)).max(read.start),
        );
    }
}

#[cfg(test)]
mod tests;
