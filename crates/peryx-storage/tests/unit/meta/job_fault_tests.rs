use std::sync::Arc;

use redb::backends::InMemoryBackend;
use rstest::rstest;

use crate::meta::fault::{self, Fault};

use super::*;

fn seeded_store() -> (MetaStore, Arc<InMemoryBackend>, Arc<Fault>, String) {
    let (inner, fault) = fault::backend();
    let store = fault::create(&inner, &fault, |write| {
        write.open_table(SERIAL)?;
        write.open_table(JOB_RUN)?;
        Ok(())
    });
    let running = store
        .start_job_run(NewJobRun {
            kind: JobKind::new("plugin_sync").unwrap(),
            scope: "running",
            repository: Some("hosted"),
            started_at_unix: 1,
        })
        .unwrap();
    for serial in 1..=8 {
        let id = store
            .start_job_run(NewJobRun {
                kind: JobKind::new("cache_refresh").unwrap(),
                scope: &format!("finished-{serial}"),
                repository: Some("hosted"),
                started_at_unix: serial + 1,
            })
            .unwrap();
        store
            .finish_job_run(&id, JobOutcome::failed(serial + 2, 1, 0, &"x".repeat(MAX_ERROR_BYTES)))
            .unwrap();
    }
    (store, inner, fault, running)
}

#[derive(Debug, Clone, Copy)]
enum Operation {
    Start,
    Finish,
    Get,
    List,
    Query,
    Recover,
    Prune,
}

fn invoke(store: &MetaStore, running: &str, operation: Operation) -> bool {
    match operation {
        Operation::Start => matches!(
            store.start_job_run(NewJobRun {
                kind: JobKind::new("cache_refresh").unwrap(),
                scope: "new",
                repository: Some("hosted"),
                started_at_unix: 30,
            }),
            Err(JobRunStoreError::Store(_))
        ),
        Operation::Finish => store.finish_job_run(running, JobOutcome::succeeded(30, 1, 1)).is_err(),
        Operation::Get => store.get_job_run(running).is_err(),
        Operation::List => store.list_job_runs().is_err(),
        Operation::Query => matches!(
            store.query_job_runs(&JobRunQuery {
                cursor: Some(running.to_owned()),
                limit: 8,
            }),
            Err(JobRunQueryError::Store(_))
        ),
        Operation::Recover => store.recover_interrupted_job_runs(30).is_err(),
        Operation::Prune => store.prune_job_runs_batch(0).is_err(),
    }
}

fn assert_atomic_result(operation: Operation, running: &str, expected: &[JobRunRecord], actual: &[JobRunRecord]) {
    let committed = match operation {
        Operation::Start => {
            let new_run = (actual[0].scope.as_str(), actual[0].state) == ("new", JobState::Running);
            let prior_runs = &actual[1..] == expected;
            actual.len() == expected.len() + 1 && new_run && prior_runs
        }
        Operation::Finish => {
            (
                actual.len() == expected.len(),
                actual.last().map(|record| (record.id.as_str(), record.state)) == Some((running, JobState::Succeeded)),
                actual[..actual.len() - 1] == expected[..expected.len() - 1],
            ) == (true, true, true)
        }
        Operation::Recover => {
            let unchanged = actual[..actual.len() - 1] == expected[..expected.len() - 1];
            (
                actual.len() == expected.len(),
                actual.last().map(|record| (record.id.as_str(), record.state)) == Some((running, JobState::Failed)),
                unchanged,
            ) == (true, true, true)
        }
        Operation::Prune => actual == &expected[expected.len() - 1..],
        Operation::Get | Operation::List | Operation::Query => false,
    };
    assert!(
        actual == expected || committed,
        "invalid durable state after {operation:?}"
    );
}

#[rstest]
#[case::start(Operation::Start)]
#[case::finish(Operation::Finish)]
#[case::get(Operation::Get)]
#[case::list(Operation::List)]
#[case::query(Operation::Query)]
#[case::recover(Operation::Recover)]
#[case::prune(Operation::Prune)]
fn test_job_run_operations_remain_atomic_after_backend_failures(#[case] operation: Operation) {
    let mut failures = 0;
    for fail_after in 0..256 {
        let (store, inner, fault, running) = seeded_store();
        let expected = store.list_job_runs().unwrap();
        drop(store);
        let store = fault::reopen(&inner, &fault);
        fault.arm(fail_after);
        if invoke(&store, &running, operation) {
            failures += 1;
            fault.disable();
            drop(store);
            let actual = fault::reopen(&inner, &fault).list_job_runs().unwrap();
            assert_atomic_result(operation, &running, &expected, &actual);
        }
    }
    assert!(failures > 0);
}

#[rstest]
#[case::start(Operation::Start)]
#[case::finish(Operation::Finish)]
#[case::recover(Operation::Recover)]
#[case::prune(Operation::Prune)]
fn test_job_run_writes_reject_a_poisoned_backend(#[case] operation: Operation) {
    let (store, inner, fault, running) = seeded_store();
    drop(store);
    let store = fault::reopen(&inner, &fault);
    fault.arm(0);
    assert!(store.get_job_run(&running).is_err());
    fault.disable();

    assert!(invoke(&store, &running, operation));
}
