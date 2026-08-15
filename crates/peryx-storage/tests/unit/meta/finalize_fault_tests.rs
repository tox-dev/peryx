use std::sync::Arc;

use redb::backends::InMemoryBackend;

use super::*;
use crate::meta::fault::{self, Fault};
use crate::meta::{
    DRIVER_KV, INGRESS_INTENT, IntentAdmission, IntentLimits, IntentPhase, JOURNAL, JOURNAL_BLOBS, JOURNAL_MUTATIONS,
    OPERATION_OUTCOME, OperationState, SERIAL,
};

const LIMITS: IntentLimits = IntentLimits {
    max_records: 1_000,
    max_bytes: 1 << 30,
    backpressure_percent: 80,
};

fn open_store(inner: &Arc<InMemoryBackend>, fault: &Arc<Fault>, initialize: bool) -> MetaStore {
    if initialize {
        fault::create(inner, fault, |write| {
            write.open_table(SERIAL)?;
            write.open_table(JOURNAL)?;
            write.open_table(JOURNAL_MUTATIONS)?;
            write.open_table(JOURNAL_BLOBS)?;
            write.open_table(DRIVER_KV)?;
            write.open_table(OPERATION_OUTCOME)?;
            write.open_table(INGRESS_INTENT)?;
            Ok(())
        })
    } else {
        fault::reopen(inner, fault)
    }
}

fn seeded() -> (MetaStore, Arc<InMemoryBackend>, Arc<Fault>) {
    let (inner, fault) = fault::backend();
    let store = open_store(&inner, &fault, true);
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

fn assert_retry(committed: bool, retry: FinalizeOutcome) {
    let replayed = match retry {
        FinalizeOutcome::Replayed(record) => {
            assert_eq!(record.response, b"response");
            true
        }
        FinalizeOutcome::Published => false,
    };
    assert_eq!(replayed, committed);
}

#[test]
fn test_commit_finalized_write_is_atomic_across_backend_failures() {
    let (store, _, _) = seeded();
    assert_eq!(finalize(&store).unwrap(), FinalizeOutcome::Published);
    assert!(assert_atomic(&store));
    assert_retry(true, finalize(&store).unwrap());

    let mut failures = 0;
    for fail_after in 0..256 {
        let (store, inner, fault) = seeded();
        drop(store);
        let store = open_store(&inner, &fault, false);
        fault.arm(fail_after);
        if finalize(&store).is_err() {
            failures += 1;
            fault.disable();
            drop(store);
            let store = open_store(&inner, &fault, false);
            let committed = assert_atomic(&store);

            let retry = finalize(&store).unwrap();
            assert_retry(committed, retry);
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
    let store = open_store(&inner, &fault, false);
    fault.arm(0);

    assert!(finalize(&store).is_err());
}
