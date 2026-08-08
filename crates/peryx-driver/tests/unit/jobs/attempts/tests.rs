use peryx_storage::meta::{FinishJobRun, JobKind, JobOutcome, JobState, MetaStore, NewJobRun};
use tokio_util::sync::CancellationToken;

use super::*;

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, store)
}

fn run() -> NewJobRun<'static> {
    NewJobRun {
        kind: JobKind::CacheRefresh,
        scope: "alpha",
        repository: None,
        started_at_unix: 100,
    }
}

#[test]
fn test_start_and_finish_owns_the_cancellation_token() {
    let (_dir, store) = store();
    let control = JobAttemptControl::new(store);
    let cancel = CancellationToken::new();
    let id = control.start(run(), cancel.clone()).unwrap();

    assert_eq!(control.cancel(&id).unwrap(), CancelJobRun::Requested);
    assert!(cancel.is_cancelled());
    assert_eq!(
        control.finish(&id, JobOutcome::succeeded(110, 2, 1)).unwrap().state,
        JobState::Succeeded
    );
    assert_eq!(control.cancel(&id).unwrap(), CancelJobRun::Finished);
}

#[test]
fn test_missing_record_keeps_the_active_attempt() {
    let (_dir, store) = store();
    let control = JobAttemptControl::new(store);
    let id = "jr_000000000000ffff";
    control.lock().insert(id.to_owned(), CancellationToken::new());

    assert!(matches!(
        control.finish(id, JobOutcome::failed(110, 0, 0, "missing")),
        Err(JobAttemptError::Missing)
    ));
    assert_eq!(control.cancel(id).unwrap(), CancelJobRun::Requested);
}

#[test]
fn test_store_error_keeps_the_active_attempt() {
    let (dir, store) = store();
    let id = store.start_job_run(run()).unwrap();
    drop(store);
    let path = dir.path().join("peryx.redb");
    let database = redb::Database::open(&path).unwrap();
    let write = database.begin_write().unwrap();
    write
        .open_table(redb::TableDefinition::<&str, &[u8]>::new("job_run"))
        .unwrap()
        .insert(id.as_str(), b"not json".as_slice())
        .unwrap();
    write.commit().unwrap();
    drop(database);
    let control = JobAttemptControl::new(MetaStore::open_existing(path).unwrap());
    let cancel = CancellationToken::new();
    control.lock().insert(id.clone(), cancel.clone());

    assert!(matches!(
        control.finish(&id, JobOutcome::failed(110, 0, 0, "failure")),
        Err(JobAttemptError::Store(_))
    ));
    assert_eq!(control.cancel(&id).unwrap(), CancelJobRun::Requested);
    assert!(cancel.is_cancelled());
}

#[test]
fn test_external_finish_releases_the_active_attempt() {
    let (_dir, store) = store();
    let control = JobAttemptControl::new(store.clone());
    let id = control.start(run(), CancellationToken::new()).unwrap();
    assert!(matches!(
        store.finish_job_run(&id, JobOutcome::succeeded(105, 0, 0)).unwrap(),
        FinishJobRun::Finished(_)
    ));

    assert!(matches!(
        control.finish(&id, JobOutcome::failed(110, 0, 0, "late")),
        Err(JobAttemptError::AlreadyFinished)
    ));
    assert_eq!(control.cancel(&id).unwrap(), CancelJobRun::Finished);
}
