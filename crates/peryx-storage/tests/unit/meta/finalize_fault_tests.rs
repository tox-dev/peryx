use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use mockall::mock;
use redb::backends::InMemoryBackend;
use redb::{Database, StorageBackend as _};

use super::*;
use crate::meta::{
    DRIVER_KV, INGRESS_INTENT, IntentAdmission, IntentLimits, IntentPhase, JOURNAL, JOURNAL_BLOBS, JOURNAL_MUTATIONS,
    MetaDatabase, OPERATION_OUTCOME, OperationState, SERIAL,
};

const LIMITS: IntentLimits = IntentLimits {
    max_records: 1_000,
    max_bytes: 1 << 30,
    backpressure_percent: 80,
};

mock! {
    #[derive(Debug)]
    Backend {}

    impl redb::StorageBackend for Backend {
        fn len(&self) -> io::Result<u64>;
        fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()>;
        fn set_len(&self, len: u64) -> io::Result<()>;
        fn sync_data(&self) -> io::Result<()>;
        fn write(&self, offset: u64, data: &[u8]) -> io::Result<()>;
    }
}

#[derive(Debug)]
struct Fault(AtomicI64);

impl Fault {
    fn disabled() -> Self {
        Self(AtomicI64::new(-1))
    }

    fn arm(&self, after: i64) {
        self.0.store(after, Ordering::SeqCst);
    }

    fn disable(&self) {
        self.arm(-1);
    }

    fn pass(&self) -> io::Result<()> {
        self.0
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| match remaining {
                -1 => Some(-1),
                0 => None,
                _ => Some(remaining - 1),
            })
            .map(drop)
            .map_err(|_| io::Error::other("injected storage failure"))
    }
}

fn backend(inner: Arc<InMemoryBackend>, fault: Arc<Fault>) -> MockBackend {
    let mut backend = MockBackend::new();
    let storage = inner.clone();
    let fail = fault.clone();
    backend
        .expect_len()
        .returning(move || fail.pass().and_then(|()| storage.len()));
    let storage = inner.clone();
    let fail = fault.clone();
    backend
        .expect_read()
        .returning(move |offset, out| fail.pass().and_then(|()| storage.read(offset, out)));
    let storage = inner.clone();
    let fail = fault.clone();
    backend
        .expect_set_len()
        .returning(move |len| fail.pass().and_then(|()| storage.set_len(len)));
    let storage = inner.clone();
    let fail = fault.clone();
    backend
        .expect_sync_data()
        .returning(move || fail.pass().and_then(|()| storage.sync_data()));
    backend
        .expect_write()
        .returning(move |offset, data| fault.pass().and_then(|()| inner.write(offset, data)));
    backend
}

fn open_store(inner: Arc<InMemoryBackend>, fault: Arc<Fault>, initialize: bool) -> MetaStore {
    // Keep tree pages crossing the mocked boundary after a fault is armed.
    let database = Database::builder()
        .set_cache_size(0)
        .create_with_backend(backend(inner, fault))
        .unwrap();
    if initialize {
        let write = database.begin_write().unwrap();
        write.open_table(SERIAL).unwrap();
        write.open_table(JOURNAL).unwrap();
        write.open_table(JOURNAL_MUTATIONS).unwrap();
        write.open_table(JOURNAL_BLOBS).unwrap();
        write.open_table(DRIVER_KV).unwrap();
        write.open_table(OPERATION_OUTCOME).unwrap();
        write.open_table(INGRESS_INTENT).unwrap();
        write.commit().unwrap();
    }
    MetaStore {
        db: Arc::new(MetaDatabase::ReadWrite(database)),
    }
}

fn seeded() -> (MetaStore, Arc<InMemoryBackend>, Arc<Fault>) {
    let inner = Arc::new(InMemoryBackend::new());
    let fault = Arc::new(Fault::disabled());
    let store = open_store(inner.clone(), fault.clone(), true);
    store
        .stage_intent(
            IntentAdmission {
                authority: "auth",
                key: "intent",
                digest: "digest",
                size: 8,
                payload: b"payload",
            },
            LIMITS,
            1,
        )
        .unwrap();
    (store, inner, fault)
}

fn finalize(store: &MetaStore) -> Result<FinalizeOutcome, MetaError> {
    let write = FinalizedWrite {
        operation: "op",
        intent_key: "intent",
        response: b"response",
        expiry_unix: Some(100),
        now: 2,
    };
    store.commit_finalized_write(write, |driver| {
        driver.put("row", b"value")?;
        Ok::<_, MetaError>(vec![b"{\"action\":\"add\"}".to_vec()])
    })
}

/// The two admissible durable states after a fault: the finalize either committed every fact or none.
fn assert_atomic(store: &MetaStore) -> bool {
    let outcome = store.operation_outcome("op").unwrap();
    let serial = store.current_serial().unwrap();
    let row = store.get_driver_value("row").unwrap();
    let phase = store.staged_intent("intent").unwrap().unwrap().phase;
    match outcome {
        None => {
            assert_eq!(serial, 0, "an aborted finalize allocates no serial");
            assert!(row.is_none(), "an aborted finalize writes no row");
            assert_eq!(
                phase,
                IntentPhase::Pending,
                "an aborted finalize leaves the intent pending"
            );
            false
        }
        Some(record) => {
            assert_eq!(record.state, OperationState::Published);
            assert_eq!(record.response, b"response");
            assert_eq!(serial, 1, "a committed finalize allocates exactly one serial");
            assert_eq!(row.as_deref(), Some(b"value".as_slice()));
            assert_eq!(phase, IntentPhase::Admitted, "a committed finalize advances the intent");
            true
        }
    }
}

#[test]
fn test_commit_finalized_write_is_atomic_across_backend_failures() {
    let mut failures = 0;
    for fail_after in 0..64 {
        let (store, inner, fault) = seeded();
        drop(store);
        let store = open_store(inner.clone(), fault.clone(), false);
        fault.arm(fail_after);
        if finalize(&store).is_err() {
            failures += 1;
            fault.disable();
            drop(store);
            let store = open_store(inner.clone(), fault.clone(), false);
            let committed = assert_atomic(&store);

            let retry = finalize(&store).unwrap();
            if committed {
                assert!(
                    matches!(retry, FinalizeOutcome::Replayed(record) if record.response == b"response"),
                    "a retry after a committed finalize replays the stored outcome"
                );
            } else {
                assert_eq!(
                    retry,
                    FinalizeOutcome::Published,
                    "a retry after an aborted finalize publishes"
                );
            }
            assert_eq!(
                store.current_serial().unwrap(),
                1,
                "the retry leaves exactly one journal entry"
            );
            assert_eq!(
                store.staged_intent("intent").unwrap().unwrap().phase,
                IntentPhase::Admitted
            );
        }
    }
    assert!(failures > 0, "the injected faults exercise the finalize boundary");
}

#[test]
fn test_commit_finalized_write_rejects_a_poisoned_backend() {
    let (store, inner, fault) = seeded();
    drop(store);
    let store = open_store(inner, fault.clone(), false);
    fault.arm(0);

    assert!(finalize(&store).is_err());
}
