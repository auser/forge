use super::*;
use crate::{ObservationBatch, ObservationStore};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

pub const MAX_OBSERVER_JOBS: usize = 4096;
const MAX_INDEX_BYTES: usize = 8 * 1024 * 1024;
const LEASE_SECONDS: u64 = 120;
pub trait ObserverClock: Send + Sync {
    fn now_unix_seconds(&self) -> u64;
}
pub struct SystemObserverClock;
impl ObserverClock for SystemObserverClock {
    fn now_unix_seconds(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }
}
#[derive(Clone, Copy, Debug, Serialize)]
pub struct BudgetLimits {
    pub session_micro_usd: u64,
    pub daily_micro_usd: u64,
}
impl Default for BudgetLimits {
    fn default() -> Self {
        Self {
            session_micro_usd: 50_000,
            daily_micro_usd: 250_000,
        }
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObserverJobState {
    Pending,
    Leased,
    Retryable,
    Failed,
    Blocked,
    Committed,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObserverLease {
    pub job: ObserverJob,
    pub token: String,
    pub expires_at: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Attempt {
    day: u64,
    reserved: u64,
    actual: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObserverJobRecord {
    pub job: ObserverJob,
    pub state: ObserverJobState,
    pub attempts: u8,
    pub next_attempt_at: u64,
    lease: Option<ObserverLease>,
    charges: Vec<Attempt>,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct ObserverStatus {
    pub pending: usize,
    pub running: usize,
    pub retryable: usize,
    pub failed: usize,
    pub blocked: usize,
    pub committed: usize,
    pub reserved_micro_usd: u64,
    pub known_micro_usd: u64,
    pub unknown_micro_usd: u64,
}
pub trait ObserverQueue: Send + Sync {
    fn enqueue(&self, job: ObserverJob) -> ObserverResult<()>;
    fn block(&self, job_id: &str) -> ObserverResult<()>;
    fn claim(
        &self,
        job_id: &str,
        reservation_micro_usd: u64,
        limits: BudgetLimits,
    ) -> ObserverResult<Option<ObserverLease>>;
    fn renew(&self, lease: &ObserverLease) -> ObserverResult<ObserverLease>;
    /// The queue gate remains held through the ledger commit. Never commit a
    /// worker result separately, even after a successful renew.
    fn finalize(
        &self,
        lease: &ObserverLease,
        batch: ValidatedObservationBatch,
        ledger: &dyn ObservationStore,
        actual_micro_usd: Option<u64>,
    ) -> ObserverResult<()>;
    fn fail(&self, lease: &ObserverLease, retryable: bool) -> ObserverResult<()>;
    /// Cancel a local attempt without discarding incurred or ambiguous spend.
    /// Existing retry bounds still apply; consent cannot reset charge history.
    fn discard(&self, lease: &ObserverLease, actual_micro_usd: Option<u64>) -> ObserverResult<()> {
        let _ = actual_micro_usd;
        self.fail(lease, true)
    }
    fn reconcile(
        &self,
        job_id: &str,
        events: &[Event],
        redactor: &Redactor,
        ledger: &dyn ObservationStore,
    ) -> ObserverResult<bool>;
    fn jobs(&self) -> ObserverResult<Vec<ObserverJobRecord>>;
    fn status(&self) -> ObserverResult<ObserverStatus>;
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    #[serde(deserialize_with = "unique_jobs")]
    jobs: BTreeMap<String, ObserverJobRecord>,
}
fn unique_jobs<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, ObserverJobRecord>, D::Error> {
    struct Unique;
    impl<'de> serde::de::Visitor<'de> for Unique {
        type Value = BTreeMap<String, ObserverJobRecord>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("bounded unique jobs")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> Result<Self::Value, A::Error> {
            let mut jobs = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, ObserverJobRecord>()? {
                if jobs.len() >= MAX_OBSERVER_JOBS || jobs.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom("invalid jobs"));
                }
            }
            Ok(jobs)
        }
    }
    deserializer.deserialize_map(Unique)
}
fn add(a: u64, b: u64) -> ObserverResult<u64> {
    a.checked_add(b).ok_or(ObserverError::LimitExceeded)
}
fn validate(index: &Index) -> ObserverResult<()> {
    if index.jobs.len() > MAX_OBSERVER_JOBS {
        return Err(ObserverError::Corrupt);
    }
    let mut leased = 0;
    let mut ranges = BTreeMap::<(&str, u64), u64>::new();
    for (id, r) in &index.jobs {
        r.job.validate().map_err(|_| ObserverError::Corrupt)?;
        if id != &r.job.id
            || r.attempts > 3
            || r.charges.len() != r.attempts as usize
            || (r.state == ObserverJobState::Leased) != r.lease.is_some()
            || (r.state == ObserverJobState::Pending && r.attempts != 0)
            || (r.state == ObserverJobState::Retryable && !(1..3).contains(&r.attempts))
            || (r.state == ObserverJobState::Failed && r.attempts == 0)
        {
            return Err(ObserverError::Corrupt);
        }
        if let Some(l) = &r.lease {
            leased += 1;
            if l.job != r.job
                || l.token.len() != 26
                || l.token.parse::<ulid::Ulid>().is_err()
                || r.attempts == 0
            {
                return Err(ObserverError::Corrupt);
            }
        }
        if ranges
            .insert((&r.job.session_id, r.job.range.start), r.job.range.end)
            .is_some()
        {
            return Err(ObserverError::Corrupt);
        }
    }
    let mut previous: Option<(&str, u64)> = None;
    for ((session, start), end) in ranges {
        if previous.is_some_and(|(s, e)| s == session && e >= start) {
            return Err(ObserverError::Corrupt);
        }
        previous = Some((session, end));
    }
    if leased > 1 {
        return Err(ObserverError::Corrupt);
    }
    status(index)?;
    Ok(())
}
fn load(files: &dyn crate::artifact::ArtifactFiles) -> ObserverResult<Index> {
    let bytes = files
        .read("index.json", MAX_INDEX_BYTES)
        .map_err(|_| ObserverError::Unavailable)?;
    decode(bytes)
}
fn decode(bytes: Option<Vec<u8>>) -> ObserverResult<Index> {
    let index = match bytes {
        Some(b) => serde_json::from_slice(&b).map_err(|_| ObserverError::Corrupt)?,
        None => Index::default(),
    };
    validate(&index)?;
    Ok(index)
}
fn save(files: &mut dyn crate::artifact::ArtifactFiles, index: &Index) -> ObserverResult<()> {
    validate(index)?;
    let bytes = serde_json::to_vec(index).map_err(|_| ObserverError::Corrupt)?;
    if bytes.len() > MAX_INDEX_BYTES {
        return Err(ObserverError::LimitExceeded);
    }
    files
        .write("index.json", &bytes)
        .map_err(|_| ObserverError::Unavailable)
}
fn status(index: &Index) -> ObserverResult<ObserverStatus> {
    let mut s = ObserverStatus::default();
    for r in index.jobs.values() {
        match r.state {
            ObserverJobState::Pending => s.pending += 1,
            ObserverJobState::Leased => s.running += 1,
            ObserverJobState::Retryable => s.retryable += 1,
            ObserverJobState::Failed => s.failed += 1,
            ObserverJobState::Blocked => s.blocked += 1,
            ObserverJobState::Committed => s.committed += 1,
        }
        for (i, c) in r.charges.iter().enumerate() {
            if let Some(actual) = c.actual {
                s.known_micro_usd = add(s.known_micro_usd, actual)?;
            } else if r.state == ObserverJobState::Leased && i + 1 == r.charges.len() {
                s.reserved_micro_usd = add(s.reserved_micro_usd, c.reserved)?;
            } else {
                s.unknown_micro_usd = add(s.unknown_micro_usd, c.reserved)?;
            }
        }
    }
    add(
        add(s.reserved_micro_usd, s.known_micro_usd)?,
        s.unknown_micro_usd,
    )?;
    Ok(s)
}
fn expire(index: &mut Index, now: u64) {
    for r in index.jobs.values_mut() {
        if r.lease.as_ref().is_some_and(|l| l.expires_at <= now) {
            r.lease = None;
            r.state = if r.attempts < 3 {
                ObserverJobState::Retryable
            } else {
                ObserverJobState::Failed
            };
            r.next_attempt_at = now;
        }
    }
}
fn fenced<'a>(
    index: &'a mut Index,
    l: &ObserverLease,
    now: u64,
) -> ObserverResult<&'a mut ObserverJobRecord> {
    let r = index
        .jobs
        .get_mut(&l.job.id)
        .ok_or(ObserverError::StaleLease)?;
    if r.state != ObserverJobState::Leased
        || !r
            .lease
            .as_ref()
            .is_some_and(|live| live.token == l.token && live.job == l.job && live.expires_at > now)
    {
        return Err(ObserverError::StaleLease);
    }
    Ok(r)
}
fn matches(job: &ObserverJob, batch: &ObservationBatch) -> bool {
    batch.source_session_id == job.session_id
        && batch.observer_job_id.as_deref() == Some(job.id.as_str())
        && batch.range == job.range
        && batch.source_fingerprint == job.source_fingerprint
        && batch.observer_version == job.policy.observer_version
}
pub struct FsObserverQueue {
    root: PathBuf,
    clock: Arc<dyn ObserverClock>,
}
impl FsObserverQueue {
    /// Project-wide charged spend today, explicitly separate from session counts.
    pub fn inspect_daily_charged(&self) -> ObserverResult<Option<u64>> {
        let Some(bytes) = crate::store::observer_read(&self.root, MAX_INDEX_BYTES)
            .map_err(|_| ObserverError::Unavailable)?
        else {
            return Ok(None);
        };
        let index = decode(Some(bytes))?;
        let day = self.clock.now_unix_seconds() / 86400;
        index
            .jobs
            .values()
            .flat_map(|r| &r.charges)
            .filter(|c| c.day == day)
            .try_fold(0, |sum, c| add(sum, c.actual.unwrap_or(c.reserved)))
            .map(Some)
    }

    /// Session-filtered persisted queue metadata; never claims or expires leases.
    pub fn inspect_session(&self, session: &str) -> ObserverResult<Option<ObserverStatus>> {
        let Some(bytes) = crate::store::observer_read(&self.root, MAX_INDEX_BYTES)
            .map_err(|_| ObserverError::Unavailable)?
        else {
            return Ok(None);
        };
        let mut index = decode(Some(bytes))?;
        expire(&mut index, self.clock.now_unix_seconds());
        index
            .jobs
            .retain(|_, record| record.job.session_id == session);
        status(&index).map(Some)
    }

    fn read_index(&self) -> ObserverResult<Index> {
        decode(
            crate::store::observer_read(&self.root, MAX_INDEX_BYTES)
                .map_err(|_| ObserverError::Unavailable)?,
        )
    }
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_clock(root, Arc::new(SystemObserverClock))
    }
    pub fn with_clock(root: impl Into<PathBuf>, clock: Arc<dyn ObserverClock>) -> Self {
        Self {
            root: root.into(),
            clock,
        }
    }
    fn transaction<T>(
        &self,
        operation: &mut dyn FnMut(&mut dyn crate::artifact::ArtifactFiles) -> ObserverResult<T>,
    ) -> ObserverResult<T> {
        crate::store::observer_transaction(&self.root, &mut |files| Ok(operation(files)))
            .map_err(|_| ObserverError::Unavailable)?
    }
}
pub struct MemoryObserverQueue {
    files: Mutex<BTreeMap<String, Vec<u8>>>,
    clock: Arc<dyn ObserverClock>,
}
impl Default for MemoryObserverQueue {
    fn default() -> Self {
        Self::with_clock(Arc::new(SystemObserverClock))
    }
}
impl MemoryObserverQueue {
    fn read_index(&self) -> ObserverResult<Index> {
        self.transaction(&mut |files| load(files))
    }
    pub fn with_clock(clock: Arc<dyn ObserverClock>) -> Self {
        Self {
            files: Mutex::new(BTreeMap::new()),
            clock,
        }
    }
    fn transaction<T>(
        &self,
        operation: &mut dyn FnMut(&mut dyn crate::artifact::ArtifactFiles) -> ObserverResult<T>,
    ) -> ObserverResult<T> {
        operation(&mut *self.files.lock().map_err(|_| ObserverError::Unavailable)?)
    }
}
macro_rules! impl_queue {
    ($ty:ty) => {
        impl ObserverQueue for $ty {
            fn block(&self, job_id: &str) -> ObserverResult<()> {
                self.transaction(&mut |files| {
                    let mut index = load(files)?;
                    expire(&mut index, self.clock.now_unix_seconds());
                    let r = index.jobs.get_mut(job_id).ok_or(ObserverError::Conflict)?;
                    if r.state == ObserverJobState::Leased {
                        return Err(ObserverError::StaleLease);
                    }
                    if r.state != ObserverJobState::Committed {
                        r.state = ObserverJobState::Blocked;
                    }
                    save(files, &index)
                })
            }
            fn enqueue(&self, job: ObserverJob) -> ObserverResult<()> {
                job.validate()?;
                self.transaction(&mut |files| {
                    let mut index = load(files)?;
                    if let Some(existing) = index.jobs.get(&job.id) {
                        return if existing.job == job {
                            Ok(())
                        } else {
                            Err(ObserverError::Conflict)
                        };
                    }
                    if index.jobs.values().any(|r| {
                        r.job.session_id == job.session_id && r.job.range.overlaps(job.range)
                    }) {
                        return Err(ObserverError::Conflict);
                    }
                    if index.jobs.len() >= MAX_OBSERVER_JOBS {
                        return Err(ObserverError::LimitExceeded);
                    }
                    index.jobs.insert(
                        job.id.clone(),
                        ObserverJobRecord {
                            job: job.clone(),
                            state: ObserverJobState::Pending,
                            attempts: 0,
                            next_attempt_at: 0,
                            lease: None,
                            charges: Vec::new(),
                        },
                    );
                    save(files, &index)
                })
            }
            fn claim(
                &self,
                job_id: &str,
                reservation_micro_usd: u64,
                limits: BudgetLimits,
            ) -> ObserverResult<Option<ObserverLease>> {
                self.transaction(&mut |files| {
                    let now = self.clock.now_unix_seconds();
                    let mut index = load(files)?;
                    expire(&mut index, now);
                    if index
                        .jobs
                        .values()
                        .any(|r| r.state == ObserverJobState::Leased)
                    {
                        save(files, &index)?;
                        return Ok(None);
                    }
                    let target = index.jobs.get(job_id).ok_or(ObserverError::Conflict)?;
                    if !matches!(
                        target.state,
                        ObserverJobState::Pending | ObserverJobState::Retryable
                    ) || target.next_attempt_at > now
                    {
                        save(files, &index)?;
                        return Ok(None);
                    }
                    let mut session_spend = 0;
                    let mut day_spend = 0;
                    for r in index.jobs.values() {
                        for c in &r.charges {
                            let charged = c.actual.unwrap_or(c.reserved);
                            if r.job.session_id == target.job.session_id {
                                session_spend = add(session_spend, charged)?;
                            }
                            if c.day == now / 86400 {
                                day_spend = add(day_spend, charged)?;
                            }
                        }
                    }
                    if add(session_spend, reservation_micro_usd)? > limits.session_micro_usd
                        || add(day_spend, reservation_micro_usd)? > limits.daily_micro_usd
                    {
                        save(files, &index)?;
                        return Err(ObserverError::BudgetExceeded);
                    }
                    let target = index.jobs.get_mut(job_id).ok_or(ObserverError::Conflict)?;
                    let lease = ObserverLease {
                        job: target.job.clone(),
                        token: ulid::Ulid::new().to_string(),
                        expires_at: add(now, LEASE_SECONDS)?,
                    };
                    target.attempts += 1;
                    target.charges.push(Attempt {
                        day: now / 86400,
                        reserved: reservation_micro_usd,
                        actual: None,
                    });
                    target.state = ObserverJobState::Leased;
                    target.lease = Some(lease.clone());
                    save(files, &index)?;
                    Ok(Some(lease))
                })
            }
            fn renew(&self, lease: &ObserverLease) -> ObserverResult<ObserverLease> {
                self.transaction(&mut |files| {
                    let mut index = load(files)?;
                    let now = self.clock.now_unix_seconds();
                    let r = fenced(&mut index, lease, now)?;
                    let renewed = ObserverLease {
                        job: lease.job.clone(),
                        token: lease.token.clone(),
                        expires_at: add(now, LEASE_SECONDS)?,
                    };
                    r.lease = Some(renewed.clone());
                    save(files, &index)?;
                    Ok(renewed)
                })
            }
            fn finalize(
                &self,
                lease: &ObserverLease,
                batch: ValidatedObservationBatch,
                ledger: &dyn ObservationStore,
                actual_micro_usd: Option<u64>,
            ) -> ObserverResult<()> {
                let mut batch = Some(batch);
                self.transaction(&mut |files| {
                    let mut index = load(files)?;
                    let r = fenced(&mut index, lease, self.clock.now_unix_seconds())?;
                    let batch = batch.take().ok_or(ObserverError::Conflict)?;
                    if !matches(&r.job, &batch.batch) {
                        return Err(ObserverError::Conflict);
                    }
                    // Gate is held until ledger and queue writes complete. A crash
                    // between them is recovered only by exact-source reconciliation.
                    ledger.commit(batch)?;
                    r.state = ObserverJobState::Committed;
                    r.lease = None;
                    if let Some(last) = r.charges.last_mut() {
                        last.actual = actual_micro_usd;
                    }
                    save(files, &index)
                })
            }
            fn fail(&self, lease: &ObserverLease, retryable: bool) -> ObserverResult<()> {
                self.transaction(&mut |files| {
                    let mut index = load(files)?;
                    let now = self.clock.now_unix_seconds();
                    let r = fenced(&mut index, lease, now)?;
                    r.state = if retryable && r.attempts < 3 {
                        ObserverJobState::Retryable
                    } else {
                        ObserverJobState::Failed
                    };
                    r.lease = None;
                    r.next_attempt_at = add(now, 5)?;
                    save(files, &index)
                })
            }
            fn discard(
                &self,
                lease: &ObserverLease,
                actual_micro_usd: Option<u64>,
            ) -> ObserverResult<()> {
                self.transaction(&mut |files| {
                    let mut index = load(files)?;
                    let now = self.clock.now_unix_seconds();
                    let r = fenced(&mut index, lease, now)?;
                    r.state = if r.attempts < 3 {
                        ObserverJobState::Retryable
                    } else {
                        ObserverJobState::Failed
                    };
                    r.lease = None;
                    r.next_attempt_at = add(now, 5)?;
                    if let Some(last) = r.charges.last_mut() {
                        last.actual = actual_micro_usd;
                    }
                    save(files, &index)
                })
            }
            fn reconcile(
                &self,
                job_id: &str,
                events: &[Event],
                redactor: &Redactor,
                ledger: &dyn ObservationStore,
            ) -> ObserverResult<bool> {
                self.transaction(&mut |files| {
                    let mut index = load(files)?;
                    let r = index.jobs.get_mut(job_id).ok_or(ObserverError::Conflict)?;
                    observer_request(&r.job, events, redactor)?;
                    let projection = ledger.projection(&r.job.session_id, events, redactor)?;
                    if projection.batches().iter().any(|b| matches(&r.job, b)) {
                        r.state = ObserverJobState::Committed;
                        r.lease = None;
                        save(files, &index)?;
                        return Ok(true);
                    }
                    if projection
                        .batches()
                        .iter()
                        .any(|b| b.range.overlaps(r.job.range))
                    {
                        r.state = ObserverJobState::Blocked;
                        r.lease = None;
                        save(files, &index)?;
                        return Err(ObserverError::Conflict);
                    }
                    Ok(false)
                })
            }
            fn jobs(&self) -> ObserverResult<Vec<ObserverJobRecord>> {
                let mut index = self.read_index()?;
                expire(&mut index, self.clock.now_unix_seconds());
                Ok(index.jobs.into_values().collect())
            }
            fn status(&self) -> ObserverResult<ObserverStatus> {
                let mut index = self.read_index()?;
                expire(&mut index, self.clock.now_unix_seconds());
                status(&index)
            }
        }
    };
}
impl_queue!(FsObserverQueue);
impl_queue!(MemoryObserverQueue);
