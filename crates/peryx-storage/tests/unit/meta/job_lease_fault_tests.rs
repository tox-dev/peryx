use std::sync::Arc;

use redb::backends::InMemoryBackend;
use rstest::rstest;

use crate::meta::fault::{self, Fault};

use super::*;

const HELD: &str = "reclaim-sweep";
const HOLDER: &str = "node-a";

fn seeded_store() -> (MetaStore, Arc<InMemoryBackend>, Arc<Fault>) {
    let (inner, fault) = fault::backend();
    let store = fault::create(&inner, &fault, |write| {
        write.open_table(JOB_LEASE)?;
        Ok(())
    });
    store.claim_job_lease(HELD, HOLDER, 1, 100, 30).unwrap();
    (store, inner, fault)
}

#[derive(Debug, Clone, Copy)]
enum Operation {
    Claim,
    Release,
    Read,
    List,
}

fn invoke(store: &MetaStore, operation: Operation) -> bool {
    match operation {
        Operation::Claim => store.claim_job_lease("fresh", "node-x", 5, 200, 30).is_err(),
        Operation::Release => store.release_job_lease(HELD, HOLDER, 1).is_err(),
        Operation::Read => store.job_lease(HELD).is_err(),
        Operation::List => store.job_leases().is_err(),
    }
}

fn assert_atomic(store: &MetaStore, operation: Operation) {
    match operation {
        Operation::Claim => {
            let lease = store.job_lease("fresh").unwrap();
            let committed = matches!(
                &lease,
                Some(held) if held.holder == "node-x" && held.epoch == 5 && held.state == LeaseState::Held
            );
            assert!(lease.is_none() || committed, "torn claim: {lease:?}");
        }
        Operation::Release => {
            let lease = store.job_lease(HELD).unwrap().expect("the held lease survives");
            assert_eq!(lease.holder, HOLDER);
            assert_eq!(lease.epoch, 1);
            assert!(matches!(lease.state, LeaseState::Held | LeaseState::Released));
        }
        Operation::Read | Operation::List => {}
    }
}

#[rstest]
#[case::claim(Operation::Claim)]
#[case::release(Operation::Release)]
#[case::read(Operation::Read)]
#[case::list(Operation::List)]
fn test_job_leases_survive_backend_failures(#[case] operation: Operation) {
    let (baseline, _, _) = seeded_store();
    match operation {
        Operation::Claim => {
            baseline.claim_job_lease("fresh", "node-x", 5, 200, 30).unwrap();
        }
        Operation::Release => {
            baseline.release_job_lease(HELD, HOLDER, 1).unwrap();
        }
        Operation::Read => {
            baseline.job_lease(HELD).unwrap();
        }
        Operation::List => {
            baseline.job_leases().unwrap();
        }
    }
    assert_atomic(&baseline, operation);

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
#[case::release(Operation::Release)]
#[case::read(Operation::Read)]
#[case::list(Operation::List)]
fn test_job_leases_reject_a_poisoned_backend(#[case] operation: Operation) {
    let (store, inner, fault) = seeded_store();
    drop(store);
    let store = fault::reopen(&inner, &fault);
    fault.arm(0);
    assert!(store.job_lease(HELD).is_err());
    fault.disable();

    assert!(invoke(&store, operation));
}

#[rstest]
#[case::read(Operation::Read)]
#[case::list(Operation::List)]
#[case::claim(Operation::Claim)]
#[case::release(Operation::Release)]
fn test_job_leases_reject_a_malformed_record(#[case] operation: Operation) {
    let (inner, fault) = fault::backend();
    let store = fault::create(&inner, &fault, |write| {
        write.open_table(JOB_LEASE)?;
        Ok(())
    });
    fault::corrupt(&store, JOB_LEASE, HELD, b"not json");

    let decodes = match operation {
        Operation::Read => matches!(store.job_lease(HELD), Err(MetaError::Decode(_))),
        Operation::List => matches!(store.job_leases(), Err(MetaError::Decode(_))),
        Operation::Claim => matches!(
            store.claim_job_lease(HELD, HOLDER, 1, 200, 30),
            Err(JobLeaseError::Store(MetaError::Decode(_)))
        ),
        Operation::Release => matches!(
            store.release_job_lease(HELD, HOLDER, 1),
            Err(JobLeaseError::Store(MetaError::Decode(_)))
        ),
    };
    assert!(decodes, "{operation:?} did not surface the decode failure");
}
