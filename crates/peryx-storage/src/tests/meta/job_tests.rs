use super::store;
use crate::meta::{JobKind, JobOutcome, JobRunRecord, JobState, NewJobRun};

#[test]
fn test_start_job_run_opens_a_running_record() {
    let (_dir, store) = store();
    let id = store
        .start_job_run(NewJobRun {
            kind: JobKind::CacheRefresh,
            scope: "hosted",
            started_at_unix: 100,
        })
        .unwrap();

    assert_eq!(
        store.get_job_run(&id).unwrap().unwrap(),
        JobRunRecord {
            id: id.clone(),
            kind: JobKind::CacheRefresh,
            scope: "hosted".to_owned(),
            state: JobState::Running,
            started_at_unix: 100,
            finished_at_unix: None,
            items_processed: 0,
            items_changed: 0,
            error: None,
        },
    );
}

#[test]
fn test_finish_job_run_records_success_and_counters() {
    let (_dir, store) = store();
    let id = store
        .start_job_run(NewJobRun {
            kind: JobKind::CacheRefresh,
            scope: "hosted",
            started_at_unix: 100,
        })
        .unwrap();

    let finished = store
        .finish_job_run(
            &id,
            JobOutcome {
                state: JobState::Succeeded,
                finished_at_unix: 142,
                items_processed: 9,
                items_changed: 3,
                error: None,
            },
        )
        .unwrap()
        .unwrap();

    assert_eq!(
        finished,
        JobRunRecord {
            id: id.clone(),
            kind: JobKind::CacheRefresh,
            scope: "hosted".to_owned(),
            state: JobState::Succeeded,
            started_at_unix: 100,
            finished_at_unix: Some(142),
            items_processed: 9,
            items_changed: 3,
            error: None,
        },
    );
    assert_eq!(store.get_job_run(&id).unwrap().unwrap(), finished);
}

#[test]
fn test_finish_job_run_records_failure_with_error() {
    let (_dir, store) = store();
    let id = store
        .start_job_run(NewJobRun {
            kind: JobKind::CacheRefresh,
            scope: "",
            started_at_unix: 100,
        })
        .unwrap();

    let failed = store
        .finish_job_run(
            &id,
            JobOutcome {
                state: JobState::Failed,
                finished_at_unix: 110,
                items_processed: 4,
                items_changed: 0,
                error: Some("upstream 503"),
            },
        )
        .unwrap()
        .unwrap();

    assert_eq!(
        failed,
        JobRunRecord {
            id,
            kind: JobKind::CacheRefresh,
            scope: String::new(),
            state: JobState::Failed,
            started_at_unix: 100,
            finished_at_unix: Some(110),
            items_processed: 4,
            items_changed: 0,
            error: Some("upstream 503".to_owned()),
        },
    );
}

#[test]
fn test_finish_job_run_ignores_unknown_id() {
    let (_dir, store) = store();
    let missing = store
        .finish_job_run(
            "jr_deadbeef",
            JobOutcome {
                state: JobState::Succeeded,
                finished_at_unix: 1,
                items_processed: 0,
                items_changed: 0,
                error: None,
            },
        )
        .unwrap();
    assert!(missing.is_none());
}

#[test]
fn test_get_job_run_absent_is_none() {
    let (_dir, store) = store();
    assert!(store.get_job_run("jr_0").unwrap().is_none());
}

#[test]
fn test_list_job_runs_returns_newest_first() {
    let (_dir, store) = store();
    assert_eq!(store.list_job_runs().unwrap(), Vec::new());

    let first = store
        .start_job_run(NewJobRun {
            kind: JobKind::CacheRefresh,
            scope: "hosted",
            started_at_unix: 10,
        })
        .unwrap();
    let second = store
        .start_job_run(NewJobRun {
            kind: JobKind::CacheRefresh,
            scope: "mirror",
            started_at_unix: 20,
        })
        .unwrap();

    let runs = store.list_job_runs().unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].id, second);
    assert_eq!(runs[1].id, first);
}
