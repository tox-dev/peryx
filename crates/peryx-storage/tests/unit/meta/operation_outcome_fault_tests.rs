use std::sync::Arc;

use redb::backends::InMemoryBackend;
use rstest::rstest;

use crate::meta::fault::{self, Fault};

use super::*;

const PENDING: &str = "op-pending";
const EXPIRED: &str = "op-expired";

fn seeded_store() -> (MetaStore, Arc<InMemoryBackend>, Arc<Fault>) {
    let (inner, fault) = fault::backend();
    let store = fault::create(&inner, &fault, |write| {
        write.open_table(OPERATION_OUTCOME)?;
        Ok(())
    });
    store.claim_operation(PENDING, Some(1_000), 1).unwrap();
    store.claim_operation(EXPIRED, Some(10), 1).unwrap();
    store
        .finalize_operation(EXPIRED, OperationResult::Published, b"done", 2)
        .unwrap();
    (store, inner, fault)
}

#[derive(Debug, Clone, Copy)]
enum Operation {
    Claim,
    Finalize,
    Read,
    Prune,
}

fn invoke(store: &MetaStore, operation: Operation) -> bool {
    match operation {
        Operation::Claim => store.claim_operation("fresh", Some(500), 30).is_err(),
        Operation::Finalize => store
            .finalize_operation(PENDING, OperationResult::Published, b"serial-9", 30)
            .is_err(),
        Operation::Read => store.operation_outcome(PENDING).is_err(),
        Operation::Prune => store.prune_operation_outcomes(1_000, 10).is_err(),
    }
}

fn assert_atomic(store: &MetaStore, operation: Operation) {
    // A fault during the durability phase can still leave a commit applied, so each write is
    // all-or-nothing: the fresh state or the prior one, never a torn record.
    match operation {
        Operation::Claim => {
            let record = store.operation_outcome("fresh").unwrap();
            assert!(
                matches!(
                    record,
                    None | Some(OperationOutcomeRecord {
                        state: OperationState::Pending,
                        expiry_unix: Some(500),
                        updated_at_unix: 30,
                        ..
                    })
                ),
                "torn claim: {record:?}"
            );
        }
        Operation::Finalize => {
            let record = store
                .operation_outcome(PENDING)
                .unwrap()
                .expect("the pending record survives");
            assert!(
                record.state == OperationState::Pending
                    || (record.state == OperationState::Published && record.response == b"serial-9"),
                "torn finalize: {record:?}"
            );
        }
        Operation::Prune => {
            if let Some(record) = store.operation_outcome(EXPIRED).unwrap() {
                assert_eq!(record.state, OperationState::Published);
                assert_eq!(record.response, b"done");
            }
        }
        Operation::Read => {}
    }
}

#[rstest]
#[case::claim(Operation::Claim)]
#[case::finalize(Operation::Finalize)]
#[case::read(Operation::Read)]
#[case::prune(Operation::Prune)]
fn test_operation_outcomes_survive_backend_failures(#[case] operation: Operation) {
    let mut failures = 0;
    for fail_after in 0..48 {
        let (store, inner, fault) = seeded_store();
        drop(store);
        let store = fault::reopen(&inner, &fault);
        fault.arm(fail_after);
        if invoke(&store, operation) {
            failures += 1;
            fault.disable();
            drop(store);
            assert_atomic(&fault::reopen(&inner, &fault), operation);
        }
    }
    assert!(failures > 0, "no backend failure surfaced for {operation:?}");
}

#[rstest]
#[case::claim(Operation::Claim)]
#[case::finalize(Operation::Finalize)]
#[case::read(Operation::Read)]
#[case::prune(Operation::Prune)]
fn test_operation_outcomes_reject_a_poisoned_backend(#[case] operation: Operation) {
    let (store, inner, fault) = seeded_store();
    drop(store);
    let store = fault::reopen(&inner, &fault);
    fault.arm(0);
    assert!(store.operation_outcome(PENDING).is_err());
    fault.disable();

    assert!(invoke(&store, operation));
}

#[rstest]
#[case::read(Operation::Read)]
#[case::claim(Operation::Claim)]
#[case::finalize(Operation::Finalize)]
#[case::prune(Operation::Prune)]
fn test_operation_outcomes_reject_a_malformed_record(#[case] operation: Operation) {
    let (inner, fault) = fault::backend();
    let store = fault::create(&inner, &fault, |write| {
        write.open_table(OPERATION_OUTCOME)?;
        Ok(())
    });
    fault::corrupt(&store, OPERATION_OUTCOME, PENDING, b"not json");

    let decodes = match operation {
        Operation::Read => matches!(store.operation_outcome(PENDING), Err(MetaError::Decode(_))),
        Operation::Claim => matches!(store.claim_operation(PENDING, Some(1), 1), Err(MetaError::Decode(_))),
        Operation::Finalize => matches!(
            store.finalize_operation(PENDING, OperationResult::Published, b"", 1),
            Err(OperationOutcomeError::Store(MetaError::Decode(_)))
        ),
        Operation::Prune => matches!(store.prune_operation_outcomes(1_000, 10), Err(MetaError::Decode(_))),
    };
    assert!(decodes, "{operation:?} did not surface the decode failure");
}
