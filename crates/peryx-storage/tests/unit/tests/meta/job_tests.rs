use rstest::rstest;

use super::store;
use crate::meta::{
    FinishJobRun, JobKind, JobOutcome, JobRunQuery, JobRunQueryError, JobRunRecord, JobRunStoreError, JobState,
    MetaStore, NewJobRun,
};

fn start_job(store: &MetaStore, scope: &str, started_at_unix: i64) -> String {
    store
        .start_job_run(NewJobRun {
            kind: JobKind::new("cache_refresh").unwrap(),
            scope,
            repository: None,
            started_at_unix,
        })
        .unwrap()
}

fn finish_job(store: &MetaStore, id: &str, finished_at_unix: i64) {
    store
        .finish_job_run(id, JobOutcome::succeeded(finished_at_unix, 0, 0))
        .unwrap();
}

#[test]
fn test_start_job_run_opens_a_running_record() {
    let (_dir, store) = store();
    let id = start_job(&store, "hosted", 100);

    assert_eq!(
        store.get_job_run(&id).unwrap().unwrap(),
        JobRunRecord {
            id: id.clone(),
            kind: JobKind::new("cache_refresh").unwrap(),
            scope: "hosted".to_owned(),
            repository: None,
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
fn test_start_job_run_records_repository_ownership() {
    let (_dir, store) = store();
    let id = store
        .start_job_run(NewJobRun {
            kind: JobKind::new("plugin_sync").unwrap(),
            scope: "mirror",
            repository: Some("hosted"),
            started_at_unix: 100,
        })
        .unwrap();

    assert_eq!(
        store.get_job_run(&id).unwrap(),
        Some(JobRunRecord {
            id,
            kind: JobKind::new("plugin_sync").unwrap(),
            scope: "mirror".to_owned(),
            repository: Some("hosted".to_owned()),
            state: JobState::Running,
            started_at_unix: 100,
            finished_at_unix: None,
            items_processed: 0,
            items_changed: 0,
            error: None,
        })
    );
}

#[test]
fn test_job_run_record_without_repository_remains_readable() {
    let record: JobRunRecord = serde_json::from_value(serde_json::json!({
        "id": "jr_0000000000000001",
        "kind": "cache_refresh",
        "scope": "alpha",
        "state": "running",
        "started_at_unix": 100,
        "finished_at_unix": null,
        "items_processed": 0,
        "items_changed": 0,
        "error": null
    }))
    .unwrap();

    assert_eq!(record.repository, None);
}

#[test]
fn test_finish_job_run_records_success_and_counters() {
    let (_dir, store) = store();
    let id = start_job(&store, "hosted", 100);

    let expected = JobRunRecord {
        id: id.clone(),
        kind: JobKind::new("cache_refresh").unwrap(),
        scope: "hosted".to_owned(),
        repository: None,
        state: JobState::Succeeded,
        started_at_unix: 100,
        finished_at_unix: Some(142),
        items_processed: 9,
        items_changed: 3,
        error: None,
    };
    assert_eq!(
        store.finish_job_run(&id, JobOutcome::succeeded(142, 9, 3)).unwrap(),
        FinishJobRun::Finished(expected.clone())
    );
    assert_eq!(store.get_job_run(&id).unwrap(), Some(expected));
}

#[test]
fn test_finish_job_run_records_failure_with_error() {
    let (_dir, store) = store();
    let id = start_job(&store, "", 100);

    let expected = JobRunRecord {
        id: id.clone(),
        kind: JobKind::new("cache_refresh").unwrap(),
        scope: String::new(),
        repository: None,
        state: JobState::Failed,
        started_at_unix: 100,
        finished_at_unix: Some(110),
        items_processed: 4,
        items_changed: 0,
        error: Some("upstream 503".to_owned()),
    };
    assert_eq!(
        store
            .finish_job_run(&id, JobOutcome::failed(110, 4, 0, "upstream 503"))
            .unwrap(),
        FinishJobRun::Finished(expected.clone())
    );
    assert_eq!(store.get_job_run(&id).unwrap(), Some(expected));
}

#[test]
fn test_finish_job_run_records_cancellation() {
    let (_dir, store) = store();
    let id = start_job(&store, "hosted", 100);

    let expected = JobRunRecord {
        id: id.clone(),
        kind: JobKind::new("cache_refresh").unwrap(),
        scope: "hosted".to_owned(),
        repository: None,
        state: JobState::Cancelled,
        started_at_unix: 100,
        finished_at_unix: Some(110),
        items_processed: 4,
        items_changed: 1,
        error: None,
    };
    assert_eq!(
        store.finish_job_run(&id, JobOutcome::cancelled(110, 4, 1)).unwrap(),
        FinishJobRun::Finished(expected.clone())
    );
    assert_eq!(store.get_job_run(&id).unwrap(), Some(expected));
}

#[test]
fn test_finish_job_run_preserves_the_first_terminal_outcome() {
    let (_dir, store) = store();
    let id = start_job(&store, "hosted", 100);
    finish_job(&store, &id, 110);
    let finished = store.get_job_run(&id).unwrap().unwrap();

    assert_eq!(
        store
            .finish_job_run(&id, JobOutcome::failed(120, 9, 3, "late failure"))
            .unwrap(),
        FinishJobRun::AlreadyFinished(finished.clone())
    );
    assert_eq!(store.get_job_run(&id).unwrap(), Some(finished));
}

#[test]
fn test_finish_job_run_ignores_unknown_id() {
    let (_dir, store) = store();
    assert_eq!(
        store
            .finish_job_run("jr_deadbeef", JobOutcome::succeeded(1, 0, 0))
            .unwrap(),
        FinishJobRun::Missing
    );
}

#[test]
fn test_get_job_run_absent_is_none() {
    let (_dir, store) = store();
    assert_eq!(store.get_job_run("jr_0").unwrap(), None);
}

#[test]
fn test_query_job_runs_paginates_newest_first_with_a_stable_cursor() {
    let (_dir, store) = store();
    let first = start_job(&store, "first", 10);
    let second = start_job(&store, "second", 20);
    let third = start_job(&store, "third", 30);
    let first_page = store.query_job_runs(&JobRunQuery { cursor: None, limit: 2 }).unwrap();
    let fourth = start_job(&store, "fourth", 40);
    let second_page = store
        .query_job_runs(&JobRunQuery {
            cursor: first_page.next_cursor.clone(),
            limit: 2,
        })
        .unwrap();

    assert_eq!(
        (
            first_page.runs.into_iter().map(|run| run.id).collect::<Vec<_>>(),
            second_page.runs.into_iter().map(|run| run.id).collect::<Vec<_>>(),
            second_page.next_cursor,
            store.list_job_runs().unwrap()[0].id.clone(),
        ),
        (vec![third, second], vec![first], None, fourth)
    );
}

#[test]
fn test_query_job_runs_accepts_a_canonical_hex_cursor() {
    let (_dir, store) = store();
    for serial in 1..=11 {
        start_job(&store, &format!("job-{serial}"), serial);
    }

    let page = store
        .query_job_runs(&JobRunQuery {
            cursor: Some("jr_000000000000000a".to_owned()),
            limit: 1,
        })
        .unwrap();

    assert_eq!(page.runs[0].id, "jr_0000000000000009");
}

#[rstest]
#[case::zero(JobRunQuery { cursor: None, limit: 0 }, "limit must be between 1 and 100")]
#[case::above_max(JobRunQuery { cursor: None, limit: 101 }, "limit must be between 1 and 100")]
#[case::malformed_cursor(JobRunQuery { cursor: Some("jr_bad".to_owned()), limit: 25 }, "invalid job run cursor")]
#[case::non_canonical_cursor(JobRunQuery { cursor: Some("jr_00000000000000FF".to_owned()), limit: 25 }, "invalid job run cursor")]
fn test_query_job_runs_rejects_invalid_bounds(#[case] query: JobRunQuery, #[case] message: &str) {
    let (_dir, store) = store();

    assert_eq!(store.query_job_runs(&query).unwrap_err().to_string(), message);
}

#[test]
fn test_start_job_run_bounds_scope_bytes_without_partial_writes() {
    let (_dir, store) = store();
    let accepted = "x".repeat(512);
    let id = start_job(&store, &accepted, 10);

    assert!(matches!(
        store.start_job_run(NewJobRun {
            kind: JobKind::new("cache_refresh").unwrap(),
            scope: &format!("{accepted}x"),
            repository: None,
            started_at_unix: 20,
        }),
        Err(JobRunStoreError::ScopeTooLong)
    ));
    assert_eq!(store.query_job_runs(&JobRunQuery::default()).unwrap().runs[0].id, id);
}

#[test]
fn test_start_job_run_bounds_repository_bytes_without_partial_writes() {
    let (_dir, store) = store();
    let accepted = "x".repeat(512);
    let id = store
        .start_job_run(NewJobRun {
            kind: JobKind::new("plugin_sync").unwrap(),
            scope: "mirror",
            repository: Some(&accepted),
            started_at_unix: 10,
        })
        .unwrap();

    assert!(matches!(
        store.start_job_run(NewJobRun {
            kind: JobKind::new("plugin_sync").unwrap(),
            scope: "mirror",
            repository: Some(&format!("{accepted}x")),
            started_at_unix: 20,
        }),
        Err(JobRunStoreError::RepositoryTooLong)
    ));
    assert_eq!(store.query_job_runs(&JobRunQuery::default()).unwrap().runs[0].id, id);
}

#[test]
fn test_finish_job_run_truncates_error_at_a_utf8_boundary() {
    let (_dir, store) = store();
    let id = start_job(&store, "hosted", 10);
    let error = format!("{}éé", "x".repeat(2_047));

    let expected = JobRunRecord {
        id: id.clone(),
        kind: JobKind::new("cache_refresh").unwrap(),
        scope: "hosted".to_owned(),
        repository: None,
        state: JobState::Failed,
        started_at_unix: 10,
        finished_at_unix: Some(20),
        items_processed: 0,
        items_changed: 0,
        error: Some("x".repeat(2_047)),
    };
    assert_eq!(
        store.finish_job_run(&id, JobOutcome::failed(20, 0, 0, &error)).unwrap(),
        FinishJobRun::Finished(expected.clone())
    );
    assert_eq!(store.get_job_run(&id).unwrap(), Some(expected));
}

#[test]
fn test_retention_removes_old_terminal_runs_and_preserves_running_runs() {
    let (_dir, store) = store();
    let ids = (0..24)
        .map(|started_at_unix| start_job(&store, "hosted", started_at_unix))
        .collect::<Vec<_>>();
    for id in &ids[..20] {
        finish_job(&store, id, 30);
    }
    assert_eq!(
        (
            store.prune_job_runs_batch(16).unwrap(),
            store.prune_job_runs_batch(16).unwrap(),
            store.prune_job_runs_batch(16).unwrap(),
        ),
        (8, 0, 0)
    );

    let runs = store
        .query_job_runs(&JobRunQuery {
            cursor: None,
            limit: 100,
        })
        .unwrap()
        .runs;
    assert_eq!(runs.len(), 16);
    assert!(runs.iter().filter(|run| run.state == JobState::Running).count() == 4);
    assert!(ids[..8].iter().all(|id| store.get_job_run(id).unwrap().is_none()));
    assert!(ids[20..].iter().all(|id| store.get_job_run(id).unwrap().is_some()));
}

#[test]
fn test_recovery_fails_all_interrupted_runs_in_bounded_batches_and_survives_restart() {
    let (dir, store) = store();
    for started_at_unix in 0..20 {
        start_job(&store, "hosted", started_at_unix);
    }
    drop(store);
    let store = MetaStore::open_existing(dir.path().join("peryx.redb")).unwrap();

    assert_eq!(store.recover_interrupted_job_runs(100).unwrap(), 20);
    assert_eq!(store.prune_job_runs_batch(16).unwrap(), 4);
    let runs = store
        .query_job_runs(&JobRunQuery {
            cursor: None,
            limit: 100,
        })
        .unwrap()
        .runs;
    assert_eq!(runs.len(), 16);
    assert!(runs.iter().all(|run| {
        run.state == JobState::Failed
            && run.finished_at_unix == Some(100)
            && run.error.as_deref() == Some("node restarted before the job finished")
    }));
}

#[test]
fn test_recovery_continues_after_a_full_batch() {
    let (_dir, store) = store();
    let ids = (0..128)
        .map(|started_at_unix| start_job(&store, "hosted", started_at_unix))
        .collect::<Vec<_>>();
    assert_eq!(store.recover_interrupted_job_runs(200).unwrap(), 128);
    assert!(
        [ids.first().unwrap(), ids.last().unwrap()]
            .into_iter()
            .all(|id| store.get_job_run(id).unwrap().unwrap().state == JobState::Failed)
    );
}

#[test]
fn test_job_run_page_serializes_attempts() {
    let (_dir, store) = store();
    start_job(&store, "hosted", 100);

    let page = serde_json::to_value(store.query_job_runs(&JobRunQuery::default()).unwrap()).unwrap();

    assert!(page.get("attempts").is_some());
    assert!(page.get("runs").is_none());
}

#[test]
fn test_query_job_runs_error_types_remain_distinct() {
    let (_dir, store) = store();

    assert!(matches!(
        store.query_job_runs(&JobRunQuery {
            cursor: Some("bad".to_owned()),
            limit: 0,
        }),
        Err(JobRunQueryError::InvalidLimit)
    ));
}

#[test]
fn test_job_run_operations_surface_an_incompatible_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let database = redb::Database::create(&path).unwrap();
    let write = database.begin_write().unwrap();
    write
        .open_table(redb::TableDefinition::<&str, u64>::new("job_run"))
        .unwrap();
    write.commit().unwrap();
    drop(database);
    let store = MetaStore::open_existing(path).unwrap();

    assert!(matches!(
        store.start_job_run(NewJobRun {
            kind: JobKind::new("cache_refresh").unwrap(),
            scope: "alpha",
            repository: None,
            started_at_unix: 1,
        }),
        Err(JobRunStoreError::Store(_))
    ));
    assert!(
        store
            .finish_job_run("jr_0000000000000001", JobOutcome::succeeded(2, 0, 0))
            .is_err()
    );
    assert!(store.get_job_run("jr_0000000000000001").is_err());
    assert!(matches!(
        store.query_job_runs(&JobRunQuery::default()),
        Err(JobRunQueryError::Store(_))
    ));
    assert!(store.list_job_runs().is_err());
    assert!(store.recover_interrupted_job_runs(2).is_err());
    assert!(store.prune_job_runs_batch(0).is_err());
}

#[test]
fn test_start_job_run_surfaces_an_incompatible_serial_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let database = redb::Database::create(&path).unwrap();
    let write = database.begin_write().unwrap();
    write
        .open_table(redb::TableDefinition::<&str, &[u8]>::new("serial"))
        .unwrap();
    write.commit().unwrap();
    drop(database);
    let store = MetaStore::open_existing(path).unwrap();

    assert!(matches!(
        store.start_job_run(NewJobRun {
            kind: JobKind::new("cache_refresh").unwrap(),
            scope: "alpha",
            repository: None,
            started_at_unix: 1,
        }),
        Err(JobRunStoreError::Store(_))
    ));
}

#[test]
fn test_job_run_operations_surface_a_corrupt_record() {
    let (dir, store) = store();
    let id = start_job(&store, "alpha", 1);
    drop(store);
    let path = dir.path().join("peryx.redb");
    let database = redb::Database::open(&path).unwrap();
    let write = database.begin_write().unwrap();
    {
        let mut table = write
            .open_table(redb::TableDefinition::<&str, &[u8]>::new("job_run"))
            .unwrap();
        table.insert(id.as_str(), b"not json".as_slice()).unwrap();
    }
    write.commit().unwrap();
    drop(database);
    let store = MetaStore::open_existing(path).unwrap();

    assert!(store.finish_job_run(&id, JobOutcome::succeeded(2, 0, 0)).is_err());
    assert!(store.get_job_run(&id).is_err());
    assert!(matches!(
        store.query_job_runs(&JobRunQuery::default()),
        Err(JobRunQueryError::Store(_))
    ));
    assert!(store.list_job_runs().is_err());
    assert!(store.recover_interrupted_job_runs(2).is_err());
    assert!(store.prune_job_runs_batch(0).is_err());
}

#[rstest]
#[case::letters("maintenance")]
#[case::digits("sync_2")]
#[case::hyphen("plugin-job")]
fn test_job_kind_roundtrips_valid_storage_keys(#[case] value: &str) {
    let kind = JobKind::new(value).unwrap();

    assert_eq!(kind.as_str(), value);
    assert_eq!(kind.to_string(), value);
    assert_eq!(
        serde_json::from_str::<JobKind>(&serde_json::to_string(&kind).unwrap()).unwrap(),
        kind
    );
}

#[rstest]
#[case::empty("")]
#[case::uppercase("Sync")]
#[case::space("plugin job")]
#[case::slash("plugin/job")]
fn test_job_kind_rejects_unstable_storage_keys(#[case] value: &str) {
    assert_eq!(
        JobKind::new(value).unwrap_err().to_string(),
        format!("invalid job kind {value:?}")
    );
    assert!(serde_json::from_str::<JobKind>(&format!("{value:?}")).is_err());
}

#[test]
fn test_job_kind_rejects_keys_over_sixty_four_bytes() {
    assert!(JobKind::new("a".repeat(65)).is_err());
}
