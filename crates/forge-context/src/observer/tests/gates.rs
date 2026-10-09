use super::*;

#[cfg(any(unix, windows))]
#[test]
fn status_is_read_only_and_expired_cost_is_unknown() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join(".forge/context");
    let c = clock();
    let q = FsObserverQueue::with_clock(&root, c.clone());
    assert_eq!(q.status().unwrap().pending, 0);
    assert!(!root.exists());
    let j = job("s");
    q.enqueue(j.clone()).unwrap();
    let l = q
        .claim(&j.id, 17, BudgetLimits::default())
        .unwrap()
        .unwrap();
    assert_eq!(q.block(&j.id), Err(ObserverError::StaleLease));
    let renewed = q.renew(&l).unwrap();
    assert_eq!(renewed.token, l.token);
    let index = root.join("observer-jobs/index.json");
    let before = std::fs::read(&index).unwrap();
    c.0.fetch_add(121, Ordering::SeqCst);
    assert_eq!(q.status().unwrap().unknown_micro_usd, 17);
    assert_eq!(before, std::fs::read(&index).unwrap());
    q.block(&j.id).unwrap();
    assert_eq!(q.status().unwrap().blocked, 1);
}

#[cfg(unix)]
#[test]
fn native_queue_rejects_symlinks_and_hardlinks() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    for component in [".forge", ".forge/context", ".forge/context/observer-jobs"] {
        let temp = tempfile::tempdir().unwrap();
        let victim = tempfile::tempdir().unwrap();
        let path = temp.path().join(component);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        symlink(victim.path(), &path).unwrap();
        let q = FsObserverQueue::new(temp.path().join(".forge/context"));
        assert!(q.enqueue(job("s")).is_err());
        assert!(q.status().is_err());
        assert_eq!(std::fs::read_dir(victim.path()).unwrap().count(), 0);
    }
    let temp = tempfile::tempdir().unwrap();
    let q = FsObserverQueue::new(temp.path());
    q.enqueue(job("s")).unwrap();
    let index = temp.path().join("observer-jobs/index.json");
    assert_eq!(
        std::fs::metadata(&index).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let victim = temp.path().join("victim");
    std::fs::hard_link(&index, &victim).unwrap();
    let original = std::fs::read(&victim).unwrap();
    assert!(q.status().is_err());
    assert!(q.enqueue(job("t")).is_err());
    assert_eq!(std::fs::read(&victim).unwrap(), original);
}

#[test]
fn queue_gate_serializes_ledger_commit_against_reclaim() {
    use std::sync::{Mutex, mpsc};
    struct BlockingLedger {
        inner: MemoryObservationStore,
        entered: mpsc::Sender<()>,
        resume: Mutex<mpsc::Receiver<()>>,
    }
    impl ObservationStore for BlockingLedger {
        fn commit(
            &self,
            batch: ValidatedObservationBatch,
        ) -> Result<crate::ObservationBatch, crate::ObservationError> {
            self.entered.send(()).unwrap();
            self.resume.lock().unwrap().recv().unwrap();
            self.inner.commit(batch)
        }
        fn fork_initialized(&self, id: &str) -> Result<bool, crate::ObservationError> {
            self.inner.fork_initialized(id)
        }
        fn projection(
            &self,
            id: &str,
            events: &[Event],
            r: &Redactor,
        ) -> Result<crate::LedgerProjection, crate::ObservationError> {
            self.inner.projection(id, events, r)
        }
        fn fork(
            &self,
            p: &str,
            ps: &[Event],
            c: &str,
            cs: &[Event],
            cut: u64,
            r: &Redactor,
        ) -> Result<crate::LedgerProjection, crate::ObservationError> {
            self.inner.fork(p, ps, c, cs, cut, r)
        }
    }
    let c = clock();
    let q = Arc::new(MemoryObserverQueue::with_clock(c.clone()));
    let j = job("s");
    q.enqueue(j.clone()).unwrap();
    let lease = q
        .claim(&j.id, 10, BudgetLimits::default())
        .unwrap()
        .unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let ledger = Arc::new(BlockingLedger {
        inner: MemoryObservationStore::default(),
        entered: entered_tx,
        resume: Mutex::new(resume_rx),
    });
    let finish = {
        let q = q.clone();
        let ledger = ledger.clone();
        let j = j.clone();
        std::thread::spawn(move || q.finalize(&lease, batch(&j), &*ledger, None))
    };
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    c.0.fetch_add(121, Ordering::SeqCst);
    let (claimed_tx, claimed_rx) = mpsc::channel();
    let claim = {
        let q = q.clone();
        let j = j.clone();
        std::thread::spawn(move || {
            claimed_tx
                .send(q.claim(&j.id, 10, BudgetLimits::default()))
                .unwrap()
        })
    };
    assert!(
        claimed_rx
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err()
    );
    resume_tx.send(()).unwrap();
    finish.join().unwrap().unwrap();
    claim.join().unwrap();
    assert!(claimed_rx.recv().unwrap().unwrap().is_none());
    assert_eq!(
        ledger
            .inner
            .projection("s", &source("s"), &Redactor::new())
            .unwrap()
            .batches()
            .len(),
        1
    );
}
