//! Behaviour tests for the node-local job scheduler and the maintenance job it runs.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use peryx_core::Ecosystem;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::{FinishJobRun, JobKind, JobOutcome, JobRunQuery, JobState, LeaseState, MetaStore, NewJobRun};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::instrument::WithSubscriber as _;

use super::attempts::{JobAttemptControl, JobAttemptError};
use super::scheduler::{JobLimits, Submit};
use super::{
    BlobReclaimer, CACHE_MAINTENANCE, CancelJobRun, CatalogSyncParameters, CrossDcCopier, DcCopyJob, DcCopyParameters,
    JobContext, JobFailure, JobHistoryCleanup, JobReport, JobScheduler, LeaseScope, MaintenanceJob, NodeJob,
    PlacementReconcileJob, PlacementReconcileParameters, PlacementReconciler, ReclamationJob, ReclamationParameters,
    Schedule, ScheduledJob, SearchRebuildJob, run_schedules, scheduled_job, submit_maintenance,
};
use crate::serving::{
    CacheRefresher, DriverCapabilities, EcosystemDriver, IdleReclaimer, JobDriver, MaintenanceCapabilities,
    MaintenanceDriver, RefreshSweep,
};
use crate::state::{AppState, Clock, ServingState};
use peryx_search::{IndexerCtx, PackageDocument, PackageIndexer, PackageSource, SearchError};

fn serving() -> (tempfile::TempDir, Arc<ServingState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let clock: Clock = Arc::new(|| 1_000);
    let state = AppState::with_clock(meta, blobs, 60, Vec::new(), clock);
    (dir, state.serving)
}

fn limits(workers: usize, queue: usize, per_kind: usize, per_repository: usize) -> JobLimits {
    let nz = |value: usize| NonZeroUsize::new(value).unwrap();
    JobLimits {
        workers: nz(workers),
        queue: nz(queue),
        per_kind: nz(per_kind),
        per_repository: nz(per_repository),
        shutdown_grace: Duration::from_secs(5),
    }
}

fn job_runs(meta: &MetaStore) -> Vec<peryx_storage::meta::JobRunRecord> {
    meta.query_job_runs(&JobRunQuery {
        cursor: None,
        limit: 100,
    })
    .unwrap()
    .runs
}

/// The behaviour a [`TestJob`] carries out once it starts running.
enum Action {
    Return(Result<JobReport, String>),
    Block(Arc<Notify>),
    UntilCancelled,
    FailWhenCancelled,
    SleepIgnoringCancel(Duration),
    FinishExternally,
    Panic,
}

/// A job with observable start and run count, for driving the scheduler through its states.
struct TestJob {
    kind: &'static str,
    scope: String,
    repository: Option<String>,
    persist: Option<JobKind>,
    action: Action,
    started: Arc<Notify>,
    ran: Arc<AtomicUsize>,
}

impl TestJob {
    fn new(kind: &'static str, scope: &str, action: Action) -> Arc<Self> {
        Arc::new(Self {
            kind,
            scope: scope.to_owned(),
            repository: None,
            persist: None,
            action,
            started: Arc::new(Notify::new()),
            ran: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn persisting(kind: &'static str, scope: &str, action: Action) -> Arc<Self> {
        Arc::new(Self {
            kind,
            scope: scope.to_owned(),
            repository: None,
            persist: Some(JobKind::CacheRefresh),
            action,
            started: Arc::new(Notify::new()),
            ran: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn persisting_repository(kind: &'static str, scope: &str, repository: &str, action: Action) -> Arc<Self> {
        Arc::new(Self {
            kind,
            scope: scope.to_owned(),
            repository: Some(repository.to_owned()),
            persist: Some(JobKind::CatalogSync),
            action,
            started: Arc::new(Notify::new()),
            ran: Arc::new(AtomicUsize::new(0)),
        })
    }
}

#[async_trait]
impl NodeJob for TestJob {
    fn kind(&self) -> &'static str {
        self.kind
    }

    fn scope(&self) -> &str {
        &self.scope
    }

    fn repository(&self) -> Option<&str> {
        self.repository.as_deref()
    }

    fn persist_as(&self) -> Option<JobKind> {
        self.persist
    }

    async fn run(&self, ctx: &JobContext) -> Result<JobReport, JobFailure> {
        self.ran.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        match &self.action {
            Action::Return(result) => result.clone().map_err(|message| JobFailure::new("test", message)),
            Action::Block(release) => {
                release.notified().await;
                Ok(JobReport::default())
            }
            Action::UntilCancelled => {
                ctx.cancelled().await;
                Ok(JobReport::default())
            }
            Action::FailWhenCancelled => {
                ctx.cancelled().await;
                Err(JobFailure::new("test", "cancelled at boundary"))
            }
            Action::SleepIgnoringCancel(duration) => {
                tokio::time::sleep(*duration).await;
                Ok(JobReport::default())
            }
            Action::FinishExternally => {
                let id = job_runs(&ctx.state().meta)
                    .into_iter()
                    .find(|run| run.state == JobState::Running)
                    .unwrap()
                    .id;
                ctx.state()
                    .meta
                    .finish_job_run(&id, JobOutcome::succeeded(1_000, 0, 0))
                    .unwrap();
                Ok(JobReport::default())
            }
            Action::Panic => panic!("test panic"),
        }
    }
}

/// An ecosystem driver whose reclaim and refresh results are fixed, counting each call.
struct StubDriver {
    ecosystem: Ecosystem,
    reclaim: usize,
    refresh: Result<RefreshSweep, String>,
    scheduled: Option<Arc<dyn NodeJob>>,
    reclaim_calls: Arc<AtomicUsize>,
    refresh_calls: Arc<AtomicUsize>,
    refresh_started: Arc<Notify>,
    /// When set, a refresh parks here after signalling `refresh_started`, so a test can hold a run
    /// in flight and observe an overlapping schedule tick being skipped.
    hold: Option<Arc<Notify>>,
}

impl StubDriver {
    fn new(reclaim: usize, refresh: Result<RefreshSweep, String>) -> Self {
        Self {
            ecosystem: Ecosystem::new("example"),
            reclaim,
            refresh,
            scheduled: None,
            reclaim_calls: Arc::new(AtomicUsize::new(0)),
            refresh_calls: Arc::new(AtomicUsize::new(0)),
            refresh_started: Arc::new(Notify::new()),
            hold: None,
        }
    }

    fn holding(reclaim: usize, refresh: Result<RefreshSweep, String>, hold: Arc<Notify>) -> Self {
        Self {
            hold: Some(hold),
            ..Self::new(reclaim, refresh)
        }
    }

    fn with_scheduled(mut self, job: Arc<dyn NodeJob>) -> Self {
        self.scheduled = Some(job);
        self
    }
}

#[async_trait]
impl EcosystemDriver for StubDriver {
    fn ecosystem(&self) -> Ecosystem {
        self.ecosystem
    }

    fn capabilities(&self) -> DriverCapabilities<'_> {
        DriverCapabilities {
            jobs: Some(self),
            ..DriverCapabilities::default()
        }
    }

    fn classify_route(&self, _path: &str) -> crate::rate_limit::RouteClass {
        crate::rate_limit::RouteClass::Listing
    }

    fn discover_index(
        &self,
        _index: crate::state::IndexDescription,
        _base: Option<&crate::discovery::BaseUrl>,
    ) -> serde_json::Value {
        serde_json::Value::Null
    }
}

impl JobDriver for StubDriver {
    fn node_job(&self, _job: &ScheduledJob) -> Option<Result<Arc<dyn NodeJob>, String>> {
        self.scheduled.clone().map(Ok)
    }
}

#[async_trait]
impl MaintenanceDriver for StubDriver {
    fn ecosystem(&self) -> Ecosystem {
        self.ecosystem
    }

    fn maintenance_capabilities(&self) -> MaintenanceCapabilities<'_> {
        MaintenanceCapabilities {
            idle_reclaimer: Some(self),
            cache_refresher: Some(self),
            ..MaintenanceCapabilities::default()
        }
    }
}

#[async_trait]
impl IdleReclaimer for StubDriver {
    async fn reclaim_idle(&self, _state: Arc<ServingState>) -> usize {
        self.reclaim_calls.fetch_add(1, Ordering::SeqCst);
        self.reclaim
    }
}

#[async_trait]
impl CacheRefresher for StubDriver {
    async fn refresh_stale(&self, _state: Arc<ServingState>) -> Result<RefreshSweep, String> {
        self.refresh_calls.fetch_add(1, Ordering::SeqCst);
        self.refresh_started.notify_one();
        if let Some(hold) = &self.hold {
            hold.notified().await;
        }
        self.refresh.clone()
    }
}

/// Poll the scheduler's exposition until `needle` appears, so a test can wait for a job to run to a
/// specific outcome without a completion signal on the job itself. A job that never reaches the
/// outcome spins here until the test's own timeout fails the run.
async fn await_metric(scheduler: &JobScheduler, needle: &str) {
    loop {
        let mut body = String::new();
        crate::state::PrometheusSource::write_metrics(scheduler.metrics().as_ref(), &mut body);
        if body.contains(needle) {
            return;
        }
        tokio::task::yield_now().await;
    }
}

fn test_subscriber() -> impl tracing::Subscriber + Send + Sync {
    tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::TRACE)
        .finish()
}

#[tokio::test]
async fn test_a_succeeding_job_runs_and_is_not_recorded_without_persistence() {
    let (_dir, state) = serving();
    let scheduler = JobScheduler::new(state.clone(), limits(2, 4, 2, 2));
    let job = TestJob::new(
        "probe",
        "a",
        Action::Return(Ok(JobReport {
            processed: 4,
            changed: 2,
        })),
    );
    assert_eq!(scheduler.submit(job.clone()), Submit::Queued);
    await_metric(
        &scheduler,
        "peryx_jobs_finished_total{kind=\"probe\",outcome=\"succeeded\"} 1",
    )
    .await;
    assert_eq!(job.ran.load(Ordering::SeqCst), 1);
    assert!(job_runs(&state.meta).is_empty());
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_run_waits_for_and_returns_a_jobs_report() {
    let (_dir, state) = serving();
    let scheduler = JobScheduler::new(state, limits(2, 4, 2, 2));
    let report = JobReport {
        processed: 4,
        changed: 2,
    };

    assert_eq!(
        scheduler
            .run(TestJob::new("probe", "a", Action::Return(Ok(report))))
            .with_subscriber(test_subscriber())
            .await
            .unwrap(),
        report
    );
    assert_eq!(
        scheduler
            .run(TestJob::new("probe", "b", Action::Return(Err("boom".to_owned()))))
            .with_subscriber(test_subscriber())
            .await
            .unwrap_err(),
        "test: boom"
    );
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_run_reports_each_admission_refusal() {
    let (_dir, state) = serving();
    let scheduler = JobScheduler::new(state, limits(1, 1, 1, 1));
    let release = Arc::new(Notify::new());
    let held = TestJob::new("probe", "a", Action::Block(release.clone()));
    scheduler.submit(held.clone());
    held.started.notified().await;

    assert_eq!(
        scheduler
            .run(TestJob::new("probe", "a", Action::Return(Ok(JobReport::default()))))
            .await
            .unwrap_err(),
        "a matching node-local job is already running"
    );
    assert_eq!(
        scheduler
            .run(TestJob::new("probe", "b", Action::Return(Ok(JobReport::default()))))
            .await
            .unwrap_err(),
        "the node-local job queue is full"
    );
    release.notify_one();
    scheduler.shutdown().await;
    assert_eq!(
        scheduler
            .run(TestJob::new("probe", "c", Action::Return(Ok(JobReport::default()))))
            .await
            .unwrap_err(),
        "the node-local job scheduler is shutting down"
    );
}

#[tokio::test]
async fn test_a_second_submission_of_the_same_kind_and_scope_conflicts() {
    let (_dir, state) = serving();
    let scheduler = JobScheduler::new(state, limits(2, 4, 2, 2));
    let release = Arc::new(Notify::new());
    let first = TestJob::new("probe", "a", Action::Block(release.clone()));
    let second = TestJob::new("probe", "a", Action::Return(Ok(JobReport::default())));
    assert_eq!(scheduler.submit(first), Submit::Queued);
    assert_eq!(scheduler.submit(second.clone()), Submit::Conflict);
    release.notify_one();
    scheduler.shutdown().await;
    assert_eq!(second.ran.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_the_same_kind_and_scope_is_admitted_again_once_the_run_finishes() {
    let (_dir, state) = serving();
    let scheduler = JobScheduler::new(state, limits(2, 4, 2, 2));
    scheduler
        .run(TestJob::new("probe", "a", Action::Return(Ok(JobReport::default()))))
        .await
        .unwrap();
    let again = TestJob::new("probe", "a", Action::Return(Ok(JobReport::default())));
    scheduler.run(again.clone()).await.unwrap();
    assert_eq!(again.ran.load(Ordering::SeqCst), 1);
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_finishing_one_scope_leaves_a_sibling_scope_of_the_same_kind_tracked() {
    let (_dir, state) = serving();
    let scheduler = JobScheduler::new(state, limits(2, 4, 2, 2));
    let hold = Arc::new(Notify::new());
    let sibling = TestJob::new("probe", "b", Action::Block(hold.clone()));
    assert_eq!(scheduler.submit(sibling.clone()), Submit::Queued);
    sibling.started.notified().await;
    scheduler
        .run(TestJob::new("probe", "a", Action::Return(Ok(JobReport::default()))))
        .await
        .unwrap();
    let sibling_again = TestJob::new("probe", "b", Action::Return(Ok(JobReport::default())));
    assert_eq!(scheduler.submit(sibling_again.clone()), Submit::Conflict);
    hold.notify_one();
    scheduler.shutdown().await;
    assert_eq!(sibling_again.ran.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_a_submission_past_a_full_queue_is_refused() {
    let (_dir, state) = serving();
    let scheduler = JobScheduler::new(state, limits(2, 1, 2, 2));
    let release = Arc::new(Notify::new());
    let first = TestJob::new("probe", "a", Action::Block(release.clone()));
    let second = TestJob::new("probe", "b", Action::Return(Ok(JobReport::default())));
    assert_eq!(scheduler.submit(first), Submit::Queued);
    assert_eq!(scheduler.submit(second.clone()), Submit::QueueFull);
    release.notify_one();
    scheduler.shutdown().await;
    assert_eq!(second.ran.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_a_per_kind_limit_serializes_runs_of_one_kind() {
    let (_dir, state) = serving();
    let scheduler = JobScheduler::new(state, limits(4, 8, 1, 4));
    let release = Arc::new(Notify::new());
    let first = TestJob::new("probe", "a", Action::Block(release.clone()));
    let second = TestJob::new("probe", "b", Action::Return(Ok(JobReport::default())));
    scheduler.submit(first.clone());
    first.started.notified().await;
    scheduler.submit(second.clone());
    assert_eq!(
        second.ran.load(Ordering::SeqCst),
        0,
        "the per-kind permit is held by the first run"
    );
    release.notify_one();
    second.started.notified().await;
    assert_eq!(second.ran.load(Ordering::SeqCst), 1);
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_a_per_repository_limit_serializes_runs_on_one_repository() {
    let (_dir, state) = serving();
    let scheduler = JobScheduler::new(state, limits(4, 8, 4, 1));
    let release = Arc::new(Notify::new());
    let first = TestJob::new("reclaim", "shared", Action::Block(release.clone()));
    let second = TestJob::new("refresh", "shared", Action::Return(Ok(JobReport::default())));
    scheduler.submit(first.clone());
    first.started.notified().await;
    scheduler.submit(second.clone());
    assert_eq!(
        second.ran.load(Ordering::SeqCst),
        0,
        "the per-repository permit is held by the first run"
    );
    release.notify_one();
    second.started.notified().await;
    assert_eq!(second.ran.load(Ordering::SeqCst), 1);
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_shutdown_cancels_a_running_job_and_skips_a_queued_one() {
    let (_dir, state) = serving();
    let scheduler = JobScheduler::new(state, limits(1, 4, 2, 2));
    let running = TestJob::new("probe", "a", Action::UntilCancelled);
    let queued = TestJob::new("probe", "b", Action::Return(Ok(JobReport::default())));
    scheduler.submit(running.clone());
    running.started.notified().await;
    scheduler.submit(queued.clone());
    assert_eq!(queued.ran.load(Ordering::SeqCst), 0);
    scheduler.shutdown().await;
    assert_eq!(running.ran.load(Ordering::SeqCst), 1);
    assert_eq!(
        queued.ran.load(Ordering::SeqCst),
        0,
        "a job admitted before shutdown never starts once cancelled"
    );
}

#[tokio::test]
async fn test_submitting_after_shutdown_is_refused() {
    let (_dir, state) = serving();
    let scheduler = JobScheduler::new(state, limits(2, 4, 2, 2));
    scheduler.shutdown().await;
    let job = TestJob::new("probe", "a", Action::Return(Ok(JobReport::default())));
    assert_eq!(scheduler.submit(job.clone()), Submit::ShuttingDown);
    assert_eq!(job.ran.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_cancel_job_run_signals_only_the_selected_attempt() {
    let (_dir, state) = serving();
    let scheduler = JobScheduler::new(state.clone(), limits(2, 4, 2, 2));
    let selected = TestJob::persisting("probe", "selected", Action::UntilCancelled);
    let other = TestJob::persisting("probe", "other", Action::UntilCancelled);
    scheduler.submit(selected.clone());
    scheduler.submit(other.clone());
    selected.started.notified().await;
    other.started.notified().await;
    let selected_id = job_runs(&state.meta)
        .into_iter()
        .find(|run| run.scope == "selected")
        .unwrap()
        .id;

    assert_eq!(scheduler.cancel_job_run(&selected_id).unwrap(), CancelJobRun::Requested);
    await_metric(
        &scheduler,
        "peryx_jobs_finished_total{kind=\"probe\",outcome=\"cancelled\"} 1",
    )
    .await;
    assert_eq!(
        state.meta.get_job_run(&selected_id).unwrap().unwrap().state,
        JobState::Cancelled
    );
    assert_eq!(
        job_runs(&state.meta)
            .into_iter()
            .find(|run| run.scope == "other")
            .unwrap()
            .state,
        JobState::Running
    );
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_cancel_job_run_records_cancelled_when_the_job_returns_an_error() {
    let (_dir, state) = serving();
    let scheduler = JobScheduler::new(state.clone(), limits(1, 2, 1, 1));
    let job = TestJob::persisting("probe", "failing", Action::FailWhenCancelled);
    scheduler.submit(job.clone());
    job.started.notified().await;
    let id = job_runs(&state.meta)[0].id.clone();

    assert_eq!(scheduler.cancel_job_run(&id).unwrap(), CancelJobRun::Requested);
    await_metric(
        &scheduler,
        "peryx_jobs_finished_total{kind=\"probe\",outcome=\"cancelled\"} 1",
    )
    .await;
    assert_eq!(
        state.meta.get_job_run(&id).unwrap().unwrap(),
        peryx_storage::meta::JobRunRecord {
            id,
            kind: JobKind::CacheRefresh,
            scope: "failing".to_owned(),
            repository: None,
            state: JobState::Cancelled,
            started_at_unix: 1_000,
            finished_at_unix: Some(1_000),
            items_processed: 0,
            items_changed: 0,
            error: None,
        }
    );
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_cancel_job_run_distinguishes_finished_and_missing_attempts() {
    let (_dir, state) = serving();
    let scheduler = JobScheduler::new(state.clone(), limits(1, 2, 1, 1));
    scheduler
        .run(TestJob::persisting(
            "probe",
            "finished",
            Action::Return(Ok(JobReport::default())),
        ))
        .await
        .unwrap();
    let id = job_runs(&state.meta)[0].id.clone();

    assert_eq!(
        (
            scheduler.cancel_job_run(&id).unwrap(),
            scheduler.cancel_job_run("jr_000000000000ffff").unwrap(),
        ),
        (CancelJobRun::Finished, CancelJobRun::Missing)
    );
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_cancel_job_run_reports_an_orphaned_running_attempt_as_unavailable() {
    let (_dir, state) = serving();
    let id = state
        .meta
        .start_job_run(NewJobRun {
            kind: JobKind::CacheRefresh,
            scope: "orphaned",
            repository: None,
            started_at_unix: 900,
        })
        .unwrap();
    let scheduler = JobScheduler::new(state, limits(1, 2, 1, 1));

    assert_eq!(scheduler.cancel_job_run(&id).unwrap(), CancelJobRun::Unavailable);
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_persisted_job_records_repository_ownership() {
    let (_dir, state) = serving();
    let scheduler = JobScheduler::new(state.clone(), limits(1, 2, 1, 1));

    scheduler
        .run(TestJob::persisting_repository(
            "catalog_sync",
            "mirror",
            "hosted",
            Action::Return(Ok(JobReport::default())),
        ))
        .await
        .unwrap();

    assert_eq!(job_runs(&state.meta)[0].repository.as_deref(), Some("hosted"));
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_persisted_job_panic_closes_the_attempt_as_failed() {
    let (_dir, state) = serving();
    let scheduler = JobScheduler::new(state.clone(), limits(1, 2, 1, 1));

    assert_eq!(
        scheduler
            .run(TestJob::persisting("probe", "panic", Action::Panic))
            .await
            .unwrap_err(),
        "job_panic: node-local job panicked"
    );

    let run = &job_runs(&state.meta)[0];
    assert_eq!(run.state, JobState::Failed);
    assert_eq!(run.error.as_deref(), Some("job_panic: node-local job panicked"));
    assert!(!state.job_attempts.is_active(&run.id));
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_persisted_job_rejects_an_external_terminal_transition() {
    let (_dir, state) = serving();
    let scheduler = JobScheduler::new(state.clone(), limits(1, 2, 1, 1));

    assert_eq!(
        scheduler
            .run(TestJob::persisting("probe", "external", Action::FinishExternally,))
            .await
            .unwrap_err(),
        "durable job attempt finished outside its active runner"
    );

    let run = &job_runs(&state.meta)[0];
    assert_eq!(run.state, JobState::Succeeded);
    assert!(!state.job_attempts.is_active(&run.id));
    scheduler.shutdown().await;
}

#[test]
fn test_attempt_control_start_and_finish_owns_the_active_token() {
    let (_dir, state) = serving();
    let cancel = CancellationToken::new();
    let id = state
        .job_attempts
        .start(
            NewJobRun {
                kind: JobKind::CacheRefresh,
                scope: "alpha",
                repository: None,
                started_at_unix: 100,
            },
            cancel,
        )
        .unwrap();

    assert!(state.job_attempts.is_active(&id));
    let record = state
        .job_attempts
        .finish(&id, JobOutcome::succeeded(110, 2, 1))
        .unwrap();
    assert_eq!(record.state, JobState::Succeeded);
    assert!(!state.job_attempts.is_active(&id));
}

#[test]
fn test_attempt_control_rejects_an_external_finish_and_releases_the_token() {
    let (_dir, state) = serving();
    let id = state
        .job_attempts
        .start(
            NewJobRun {
                kind: JobKind::CacheRefresh,
                scope: "alpha",
                repository: None,
                started_at_unix: 100,
            },
            CancellationToken::new(),
        )
        .unwrap();
    assert!(matches!(
        state
            .meta
            .finish_job_run(&id, JobOutcome::succeeded(105, 0, 0))
            .unwrap(),
        FinishJobRun::Finished(_)
    ));

    assert!(matches!(
        state.job_attempts.finish(&id, JobOutcome::failed(110, 0, 0, "late")),
        Err(JobAttemptError::AlreadyFinished)
    ));
    assert!(!state.job_attempts.is_active(&id));
}

#[test]
fn test_attempt_control_retains_a_missing_active_attempt() {
    let (_dir, state) = serving();
    let id = "jr_000000000000ffff";
    state.job_attempts.activate(id.to_owned(), CancellationToken::new());

    assert!(matches!(
        state.job_attempts.finish(id, JobOutcome::failed(110, 0, 0, "missing")),
        Err(JobAttemptError::Missing)
    ));
    assert!(state.job_attempts.is_active(id));
}

#[test]
fn test_attempt_control_retains_the_token_when_the_store_rejects_finish() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let store = MetaStore::open(&path).unwrap();
    let id = start_corruptible_attempt(&store);
    drop(store);
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
    let control = JobAttemptControl::new(MetaStore::open_existing(path).unwrap());
    assert!(control.cancel(&id).is_err());
    control.activate(id.clone(), CancellationToken::new());

    assert!(matches!(
        control.finish(&id, JobOutcome::failed(110, 0, 0, "failure")),
        Err(JobAttemptError::Store(_))
    ));
    assert!(control.is_active(&id));
}

fn start_corruptible_attempt(store: &MetaStore) -> String {
    store
        .start_job_run(NewJobRun {
            kind: JobKind::CacheRefresh,
            scope: "alpha",
            repository: None,
            started_at_unix: 100,
        })
        .unwrap()
}

#[tokio::test]
async fn test_run_rejects_an_oversized_persisted_scope_before_running_the_job() {
    let (_dir, state) = serving();
    let scheduler = JobScheduler::new(state, limits(1, 2, 1, 1));
    let job = TestJob::persisting("probe", &"x".repeat(513), Action::Return(Ok(JobReport::default())));

    assert_eq!(
        (
            scheduler.run(job.clone()).await.unwrap_err(),
            job.ran.load(Ordering::SeqCst)
        ),
        ("scope exceeds 512 bytes".to_owned(), 0)
    );
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_recovering_scheduler_fails_an_interrupted_attempt_before_accepting_work() {
    let (_dir, state) = serving();
    let id = state
        .meta
        .start_job_run(NewJobRun {
            kind: JobKind::CacheRefresh,
            scope: "alpha",
            repository: None,
            started_at_unix: 900,
        })
        .unwrap();

    state.job_attempts.recover_interrupted((state.clock)()).unwrap();
    let scheduler = JobScheduler::new(state.clone(), limits(1, 2, 1, 1));

    assert_eq!(
        state.meta.get_job_run(&id).unwrap().unwrap(),
        peryx_storage::meta::JobRunRecord {
            id,
            kind: JobKind::CacheRefresh,
            scope: "alpha".to_owned(),
            repository: None,
            state: JobState::Failed,
            started_at_unix: 900,
            finished_at_unix: Some(1_000),
            items_processed: 0,
            items_changed: 0,
            error: Some("node restarted before the job finished".to_owned()),
        }
    );
    scheduler.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn test_shutdown_returns_after_the_grace_period_when_a_job_ignores_cancellation() {
    let (_dir, state) = serving();
    let mut limits = limits(2, 4, 2, 2);
    limits.shutdown_grace = Duration::from_millis(50);
    let scheduler = JobScheduler::new(state, limits);
    let stubborn = TestJob::new("probe", "a", Action::SleepIgnoringCancel(Duration::from_secs(30)));
    scheduler.submit(stubborn.clone());
    stubborn.started.notified().await;
    // Paused time makes this exact: shutdown must return after the 50ms grace, not the 30s job. The
    // virtual clock advances only to whichever timer fires first, so a bound between the two proves the
    // grace won without racing a wall clock.
    let start = tokio::time::Instant::now();
    scheduler.shutdown().await;
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "shutdown waited on the stubborn job past its grace"
    );
}

#[tokio::test]
async fn test_a_failing_persisted_job_records_a_failed_run() {
    let (_dir, state) = serving();
    let scheduler = JobScheduler::new(state.clone(), limits(2, 4, 2, 2));
    let job = TestJob::persisting("cache_maintenance", "alpha", Action::Return(Err("boom".to_owned())));
    scheduler.submit(job.clone());
    job.started.notified().await;
    scheduler.shutdown().await;
    let runs = job_runs(&state.meta);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].state, JobState::Failed);
    assert_eq!(runs[0].error.as_deref(), Some("test: boom"));
}

#[tokio::test]
async fn test_metrics_expose_a_kinds_full_lifecycle_series() {
    let (_dir, state) = serving();
    let scheduler = JobScheduler::new(state, limits(2, 4, 2, 2));
    let job = TestJob::new("probe", "a", Action::Return(Ok(JobReport::default())));
    scheduler.submit(job.clone());
    await_metric(
        &scheduler,
        "peryx_jobs_finished_total{kind=\"probe\",outcome=\"succeeded\"} 1",
    )
    .await;
    scheduler.shutdown().await;
    let mut body = String::new();
    crate::state::PrometheusSource::write_metrics(scheduler.metrics().as_ref(), &mut body);
    assert!(body.contains("peryx_jobs_started_total{kind=\"probe\"} 1"));
    assert!(body.contains("peryx_jobs_finished_total{kind=\"probe\",outcome=\"succeeded\"} 1"));
    assert!(body.contains("peryx_jobs_running{kind=\"probe\"} 0"));
}

fn context(state: Arc<ServingState>, cancel: CancellationToken) -> JobContext {
    JobContext {
        state,
        cancel,
        fence: 0,
    }
}

#[tokio::test]
async fn test_maintenance_reclaims_then_refreshes_and_reports_the_sweep() {
    let (_dir, state) = serving();
    let driver = Arc::new(StubDriver::new(2, Ok(RefreshSweep { checked: 3, changed: 1 })));
    let reclaim_calls = driver.reclaim_calls.clone();
    let job = MaintenanceJob { driver };
    let report = job.run(&context(state, CancellationToken::new())).await.unwrap();
    assert_eq!(
        report,
        JobReport {
            processed: 3,
            changed: 1
        }
    );
    assert_eq!(reclaim_calls.load(Ordering::SeqCst), 1);
    assert_eq!(job.kind(), CACHE_MAINTENANCE);
    assert_eq!(job.scope(), "example");
    assert_eq!(job.persist_as(), Some(JobKind::CacheRefresh));
}

#[tokio::test]
async fn test_maintenance_with_no_work_reports_nothing() {
    let (_dir, state) = serving();
    let job = MaintenanceJob {
        driver: Arc::new(StubDriver::new(0, Ok(RefreshSweep::default()))),
    };
    assert_eq!(
        job.run(&context(state, CancellationToken::new())).await.unwrap(),
        JobReport::default()
    );
}

#[tokio::test]
async fn test_maintenance_propagates_a_refresh_failure() {
    let (_dir, state) = serving();
    let job = MaintenanceJob {
        driver: Arc::new(StubDriver::new(0, Err("upstream down".to_owned()))),
    };
    assert_eq!(
        job.run(&context(state, CancellationToken::new()))
            .await
            .unwrap_err()
            .to_string(),
        "cache_refresh: upstream down"
    );
}

#[tokio::test]
async fn test_maintenance_skips_the_refresh_when_cancelled_after_reclaim() {
    let (_dir, state) = serving();
    let driver = Arc::new(StubDriver::new(1, Ok(RefreshSweep { checked: 9, changed: 9 })));
    let refresh_calls = driver.refresh_calls.clone();
    let job = MaintenanceJob { driver };
    let cancel = CancellationToken::new();
    cancel.cancel();
    let report = job.run(&context(state, cancel)).await.unwrap();
    assert_eq!(report, JobReport::default());
    assert_eq!(
        refresh_calls.load(Ordering::SeqCst),
        0,
        "a cancelled pass reclaims but does not sweep"
    );
}

#[tokio::test]
async fn test_submit_maintenance_runs_one_job_per_driver_and_records_it() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let clock: Clock = Arc::new(|| 1_000);
    let mut state = AppState::with_clock(meta, blobs, 60, Vec::new(), clock);
    let driver = Arc::new(StubDriver::new(1, Ok(RefreshSweep { checked: 2, changed: 1 })));
    let refresh_started = driver.refresh_started.clone();
    state.register_ecosystem(driver.clone(), Arc::new(peryx_search::EmptyIndexer));
    state.register_maintenance_driver(Ecosystem::new("example"), driver);
    let scheduler = JobScheduler::new(state.serving.clone(), JobLimits::node_local());
    submit_maintenance(&state, &scheduler);
    refresh_started.notified().await;
    scheduler.shutdown().await;
    let runs = job_runs(&state.serving.meta);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].kind, JobKind::CacheRefresh);
    assert_eq!(runs[0].scope, "example");
    assert_eq!(runs[0].state, JobState::Succeeded);
    assert_eq!(runs[0].items_processed, 2);
    assert_eq!(runs[0].items_changed, 1);
}

/// A job that leans on every default the trait provides, so the `persist_as` default is exercised.
struct BareJob {
    scope: String,
}

#[async_trait]
impl NodeJob for BareJob {
    fn kind(&self) -> &'static str {
        "bare"
    }

    fn scope(&self) -> &str {
        &self.scope
    }

    async fn run(&self, _ctx: &JobContext) -> Result<JobReport, JobFailure> {
        Ok(JobReport::default())
    }
}

#[test]
fn test_a_job_defaults_to_running_without_a_persisted_record() {
    let job = BareJob { scope: String::new() };
    assert_eq!((job.repository(), job.persist_as()), (None, None));
}

struct NoScheduledJobsDriver;

#[async_trait]
impl EcosystemDriver for NoScheduledJobsDriver {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::new("example")
    }

    fn classify_route(&self, _path: &str) -> crate::rate_limit::RouteClass {
        crate::rate_limit::RouteClass::Listing
    }

    fn discover_index(
        &self,
        _index: crate::state::IndexDescription,
        _base: Option<&crate::discovery::BaseUrl>,
    ) -> serde_json::Value {
        serde_json::Value::Null
    }
}

#[test]
fn test_a_driver_without_scheduled_jobs_declines_by_default() {
    assert!(NoScheduledJobsDriver.capabilities().jobs.is_none());
}

#[test]
fn test_job_failure_exposes_its_stable_category_and_safe_message() {
    let failure = JobFailure::new("retryable_timeout", "catalog request timed out");

    assert_eq!(failure.code(), "retryable_timeout");
    assert_eq!(failure.message(), "catalog request timed out");
    assert_eq!(failure.to_string(), "retryable_timeout: catalog request timed out");
}

#[tokio::test]
async fn test_job_history_cleanup_removes_every_excess_terminal_attempt() {
    let (_dir, state) = serving();
    for started_at_unix in 0..24 {
        let id = start_corruptible_attempt(&state.meta);
        assert!(matches!(
            state
                .meta
                .finish_job_run(&id, JobOutcome::succeeded(started_at_unix, 0, 0))
                .unwrap(),
            FinishJobRun::Finished(_)
        ));
    }

    let report = JobHistoryCleanup
        .run(&context(state.clone(), CancellationToken::new()))
        .await
        .unwrap();

    assert_eq!(
        report,
        JobReport {
            processed: 8,
            changed: 8
        }
    );
    assert_eq!(job_runs(&state.meta).len(), 16);
}

#[tokio::test]
async fn test_job_history_cleanup_honors_cancellation_before_writing() {
    let (_dir, state) = serving();
    for _ in 0..17 {
        let id = start_corruptible_attempt(&state.meta);
        state
            .meta
            .finish_job_run(&id, JobOutcome::succeeded(100, 0, 0))
            .unwrap();
    }
    let cancel = CancellationToken::new();
    cancel.cancel();

    let report = JobHistoryCleanup.run(&context(state.clone(), cancel)).await.unwrap();

    assert_eq!(report, JobReport::default());
    assert_eq!(job_runs(&state.meta).len(), 17);
}

#[tokio::test]
async fn test_job_history_cleanup_categorizes_storage_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let store = MetaStore::open(&path).unwrap();
    let ids = (0..17).map(|_| start_corruptible_attempt(&store)).collect::<Vec<_>>();
    for id in &ids {
        store.finish_job_run(id, JobOutcome::succeeded(100, 0, 0)).unwrap();
    }
    drop(store);
    let database = redb::Database::open(&path).unwrap();
    let write = database.begin_write().unwrap();
    {
        let mut table = write
            .open_table(redb::TableDefinition::<&str, &[u8]>::new("job_run"))
            .unwrap();
        table.insert(ids[0].as_str(), b"not json".as_slice()).unwrap();
    }
    write.commit().unwrap();
    drop(database);
    let state = AppState::with_clock(
        MetaStore::open_existing(path).unwrap(),
        BlobStore::new(dir.path().join("blobs")),
        60,
        Vec::new(),
        Arc::new(|| 1_000),
    );

    let error = JobHistoryCleanup
        .run(&context(state.serving, CancellationToken::new()))
        .await
        .unwrap_err();

    assert_eq!(error.code(), "storage");
}

fn scheduled_app(driver: Arc<StubDriver>) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let clock: Clock = Arc::new(|| 1_000);
    let mut state = AppState::with_clock(meta, blobs, 60, Vec::new(), clock);
    state.register_ecosystem(driver.clone(), Arc::new(peryx_search::EmptyIndexer));
    state.register_maintenance_driver(Ecosystem::new("example"), driver);
    (dir, Arc::new(state))
}

fn cache_schedule(secs: u64) -> Vec<Schedule> {
    vec![Schedule {
        job: ScheduledJob::CacheMaintenance,
        interval: Duration::from_secs(secs),
    }]
}

#[test]
fn test_a_scheduled_job_reports_its_stable_label() {
    assert_eq!(ScheduledJob::CacheMaintenance.as_str(), CACHE_MAINTENANCE);
    assert_eq!(ScheduledJob::DcCopy(DcCopyParameters::new()).as_str(), "dc_copy");
}

#[test]
fn test_dc_copy_parameters_default_to_the_bounded_concurrency() {
    assert_eq!(DcCopyParameters::default(), DcCopyParameters::new());
    assert_eq!(
        DcCopyParameters::new().concurrency.get(),
        super::DEFAULT_DC_COPY_CONCURRENCY
    );
}

struct StubCopier {
    ran: Arc<AtomicUsize>,
    report: JobReport,
}

#[async_trait]
impl CrossDcCopier for StubCopier {
    async fn copy_pass(
        &self,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
        _concurrency: std::num::NonZeroUsize,
    ) -> Result<peryx_ha::AvailabilityTaskReport, peryx_ha::AvailabilityTaskError> {
        assert!(!cancelled(), "the pass observes the cooperative cancellation signal");
        self.ran.fetch_add(1, Ordering::SeqCst);
        Ok(peryx_ha::AvailabilityTaskReport {
            processed: self.report.processed,
            changed: self.report.changed,
        })
    }
}

#[tokio::test]
async fn test_dc_copy_job_delegates_to_the_registered_copier() {
    let (_dir, state) = serving();
    let ran = Arc::new(AtomicUsize::new(0));
    state.set_cross_dc_copier(Arc::new(StubCopier {
        ran: ran.clone(),
        report: JobReport {
            processed: 3,
            changed: 2,
        },
    }));
    let job = DcCopyJob {
        parameters: DcCopyParameters::new(),
    };
    assert_eq!(job.kind(), "dc_copy");
    assert_eq!(job.scope(), "");
    let report = job.run(&context(state, CancellationToken::new())).await.unwrap();

    assert_eq!(
        report,
        JobReport {
            processed: 3,
            changed: 2
        }
    );
    assert_eq!(ran.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_dc_copy_job_is_a_no_op_without_a_registered_copier() {
    let (_dir, state) = serving();
    let job = DcCopyJob {
        parameters: DcCopyParameters::new(),
    };

    let report = job.run(&context(state, CancellationToken::new())).await.unwrap();

    assert_eq!(report, JobReport::default());
}

fn dc_copy_schedule(secs: u64) -> Vec<Schedule> {
    vec![Schedule {
        job: ScheduledJob::DcCopy(DcCopyParameters::new()),
        interval: Duration::from_secs(secs),
    }]
}

#[tokio::test(start_paused = true)]
async fn test_a_dc_copy_schedule_submits_the_registered_copier() {
    let (_dir, app) = scheduled_app(Arc::new(StubDriver::new(0, Ok(RefreshSweep::default()))));
    let ran = Arc::new(AtomicUsize::new(0));
    app.serving.set_cross_dc_copier(Arc::new(StubCopier {
        ran: ran.clone(),
        report: JobReport::default(),
    }));
    let scheduler = Arc::new(JobScheduler::new(app.serving.clone(), JobLimits::node_local()));
    let cancel = CancellationToken::new();
    let timer = tokio::spawn(run_schedules(
        app.clone(),
        scheduler.clone(),
        dc_copy_schedule(60),
        cancel.clone(),
    ));
    tokio::task::yield_now().await;

    tokio::time::advance(Duration::from_mins(1)).await;
    await_metric(
        &scheduler,
        "peryx_jobs_finished_total{kind=\"dc_copy\",outcome=\"succeeded\"} 1",
    )
    .await;

    cancel.cancel();
    timer.await.unwrap();
    scheduler.shutdown().await;
    assert_eq!(ran.load(Ordering::SeqCst), 1);
}

struct StubReconciler {
    ran: Arc<AtomicUsize>,
    report: JobReport,
}

#[async_trait]
impl PlacementReconciler for StubReconciler {
    async fn reconcile_pass(
        &self,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
        _batch: std::num::NonZeroUsize,
    ) -> Result<peryx_ha::AvailabilityTaskReport, peryx_ha::AvailabilityTaskError> {
        assert!(!cancelled(), "the pass observes the cooperative cancellation signal");
        self.ran.fetch_add(1, Ordering::SeqCst);
        Ok(peryx_ha::AvailabilityTaskReport {
            processed: self.report.processed,
            changed: self.report.changed,
        })
    }
}

#[test]
fn test_placement_reconcile_parameters_default_matches_new() {
    assert_eq!(
        PlacementReconcileParameters::default(),
        PlacementReconcileParameters::new()
    );
}

#[test]
fn test_a_placement_reconcile_kind_names_itself() {
    assert_eq!(
        ScheduledJob::PlacementReconcile(PlacementReconcileParameters::new()).as_str(),
        "placement_reconcile"
    );
}

#[tokio::test]
async fn test_placement_reconcile_job_delegates_to_the_registered_reconciler() {
    let (_dir, state) = serving();
    let ran = Arc::new(AtomicUsize::new(0));
    state.set_placement_reconciler(Arc::new(StubReconciler {
        ran: ran.clone(),
        report: JobReport {
            processed: 4,
            changed: 1,
        },
    }));
    let job = PlacementReconcileJob {
        parameters: PlacementReconcileParameters::new(),
    };
    assert_eq!(job.kind(), "placement_reconcile");
    assert_eq!(job.scope(), "");

    let report = job.run(&context(state, CancellationToken::new())).await.unwrap();

    assert_eq!(
        report,
        JobReport {
            processed: 4,
            changed: 1
        }
    );
    assert_eq!(ran.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_placement_reconcile_job_is_a_no_op_without_a_registered_reconciler() {
    let (_dir, state) = serving();
    let job = PlacementReconcileJob {
        parameters: PlacementReconcileParameters::new(),
    };

    let report = job.run(&context(state, CancellationToken::new())).await.unwrap();

    assert_eq!(report, JobReport::default());
}

#[tokio::test(start_paused = true)]
async fn test_a_placement_reconcile_schedule_submits_the_registered_reconciler() {
    let (_dir, app) = scheduled_app(Arc::new(StubDriver::new(0, Ok(RefreshSweep::default()))));
    let ran = Arc::new(AtomicUsize::new(0));
    app.serving.set_placement_reconciler(Arc::new(StubReconciler {
        ran: ran.clone(),
        report: JobReport::default(),
    }));
    let scheduler = Arc::new(JobScheduler::new(app.serving.clone(), JobLimits::node_local()));
    let cancel = CancellationToken::new();
    let plan = vec![Schedule {
        job: ScheduledJob::PlacementReconcile(PlacementReconcileParameters::new()),
        interval: Duration::from_mins(1),
    }];
    let timer = tokio::spawn(run_schedules(app.clone(), scheduler.clone(), plan, cancel.clone()));
    tokio::task::yield_now().await;

    tokio::time::advance(Duration::from_mins(1)).await;
    await_metric(
        &scheduler,
        "peryx_jobs_finished_total{kind=\"placement_reconcile\",outcome=\"succeeded\"} 1",
    )
    .await;

    cancel.cancel();
    timer.await.unwrap();
    scheduler.shutdown().await;
    assert_eq!(ran.load(Ordering::SeqCst), 1);
}

struct StubReclaimer {
    ran: Arc<AtomicUsize>,
    seen_fence: Arc<AtomicU64>,
    report: JobReport,
}

#[async_trait]
impl BlobReclaimer for StubReclaimer {
    async fn reclaim_pass(
        &self,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
        fence: u64,
        _batch: std::num::NonZeroUsize,
    ) -> Result<peryx_ha::AvailabilityTaskReport, peryx_ha::AvailabilityTaskError> {
        assert!(!cancelled(), "the pass observes the cooperative cancellation signal");
        self.ran.fetch_add(1, Ordering::SeqCst);
        self.seen_fence.store(fence, Ordering::SeqCst);
        Ok(peryx_ha::AvailabilityTaskReport {
            processed: self.report.processed,
            changed: self.report.changed,
        })
    }
}

#[test]
fn test_reclamation_parameters_default_to_the_bounded_batch() {
    assert_eq!(ReclamationParameters::default(), ReclamationParameters::new());
    assert_eq!(
        ReclamationParameters::new().batch.get(),
        super::DEFAULT_RECLAMATION_BATCH
    );
}

#[test]
fn test_a_reclamation_kind_reports_its_label() {
    assert_eq!(
        ScheduledJob::Reclamation(ReclamationParameters::new()).as_str(),
        "reclamation"
    );
}

#[tokio::test]
async fn test_reclamation_job_delegates_to_the_registered_reclaimer_under_the_fence() {
    let (_dir, state) = serving();
    let ran = Arc::new(AtomicUsize::new(0));
    let seen_fence = Arc::new(AtomicU64::new(0));
    state.set_blob_reclaimer(Arc::new(StubReclaimer {
        ran: ran.clone(),
        seen_fence: seen_fence.clone(),
        report: JobReport {
            processed: 5,
            changed: 2,
        },
    }));
    let job = ReclamationJob {
        parameters: ReclamationParameters::new(),
    };
    assert_eq!(job.kind(), "reclamation");
    assert_eq!(job.scope(), "");
    assert!(
        job.repository().is_none(),
        "a node-wide reclamation pass names no repository"
    );
    assert_eq!(
        job.lease_scope(),
        LeaseScope::ClusterSingleton("reclamation".to_owned())
    );

    let context = JobContext {
        state,
        cancel: CancellationToken::new(),
        fence: 7,
    };
    let report = job.run(&context).await.unwrap();

    assert_eq!(
        report,
        JobReport {
            processed: 5,
            changed: 2
        }
    );
    assert_eq!(ran.load(Ordering::SeqCst), 1);
    assert_eq!(
        seen_fence.load(Ordering::SeqCst),
        7,
        "the run passes its lease term as the fence"
    );
}

#[tokio::test]
async fn test_reclamation_job_is_a_no_op_without_a_registered_reclaimer() {
    let (_dir, state) = serving();
    let job = ReclamationJob {
        parameters: ReclamationParameters::new(),
    };

    let report = job.run(&context(state, CancellationToken::new())).await.unwrap();

    assert_eq!(report, JobReport::default());
}

#[tokio::test(start_paused = true)]
async fn test_a_reclamation_schedule_submits_the_registered_reclaimer() {
    let (_dir, app) = scheduled_app(Arc::new(StubDriver::new(0, Ok(RefreshSweep::default()))));
    let ran = Arc::new(AtomicUsize::new(0));
    app.serving.set_blob_reclaimer(Arc::new(StubReclaimer {
        ran: ran.clone(),
        seen_fence: Arc::new(AtomicU64::new(0)),
        report: JobReport::default(),
    }));
    let scheduler = Arc::new(JobScheduler::new(app.serving.clone(), JobLimits::node_local()));
    let cancel = CancellationToken::new();
    let plan = vec![Schedule {
        job: ScheduledJob::Reclamation(ReclamationParameters::new()),
        interval: Duration::from_mins(1),
    }];
    let timer = tokio::spawn(run_schedules(app.clone(), scheduler.clone(), plan, cancel.clone()));
    tokio::task::yield_now().await;

    tokio::time::advance(Duration::from_mins(1)).await;
    await_metric(
        &scheduler,
        "peryx_jobs_finished_total{kind=\"reclamation\",outcome=\"succeeded\"} 1",
    )
    .await;

    cancel.cancel();
    timer.await.unwrap();
    scheduler.shutdown().await;
    assert_eq!(ran.load(Ordering::SeqCst), 1);
}

fn catalog_schedule(secs: u64) -> Vec<Schedule> {
    vec![Schedule {
        job: ScheduledJob::CatalogSync(CatalogSyncParameters {
            repository: "alpha".to_owned(),
            source: None,
            max_projects: NonZeroUsize::new(1).unwrap(),
            concurrency: NonZeroUsize::new(1).unwrap(),
            timeout: Duration::from_secs(1),
        }),
        interval: Duration::from_secs(secs),
    }]
}

#[tokio::test(start_paused = true)]
async fn test_an_unsupported_scheduled_job_is_rejected_without_submission() {
    let (_dir, app) = scheduled_app(Arc::new(StubDriver::new(0, Ok(RefreshSweep::default()))));
    let plan = catalog_schedule(60);
    let Err(error) = scheduled_job(&app, &plan[0].job) else {
        panic!("the stub driver must decline catalog sync")
    };
    assert_eq!(error, "no installed ecosystem supports catalog_sync");
    let scheduler = Arc::new(JobScheduler::new(app.serving.clone(), JobLimits::node_local()));
    let cancel = CancellationToken::new();
    let timer = tokio::spawn(
        run_schedules(app.clone(), scheduler.clone(), plan, cancel.clone()).with_subscriber(test_subscriber()),
    );
    tokio::task::yield_now().await;

    tokio::time::advance(Duration::from_mins(1)).await;
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }

    cancel.cancel();
    timer.await.unwrap();
    scheduler.shutdown().await;
    assert!(job_runs(&app.meta).is_empty());
}

#[tokio::test(start_paused = true)]
async fn test_a_supported_scheduled_job_is_submitted() {
    let job = TestJob::new("catalog_sync", "alpha", Action::Return(Ok(JobReport::default())));
    let driver = StubDriver::new(0, Ok(RefreshSweep::default())).with_scheduled(job.clone());
    let (_dir, app) = scheduled_app(Arc::new(driver));
    let scheduler = Arc::new(JobScheduler::new(app.serving.clone(), JobLimits::node_local()));
    let cancel = CancellationToken::new();
    let timer = tokio::spawn(run_schedules(
        app.clone(),
        scheduler.clone(),
        catalog_schedule(60),
        cancel.clone(),
    ));
    tokio::task::yield_now().await;

    tokio::time::advance(Duration::from_mins(1)).await;
    await_metric(
        &scheduler,
        "peryx_jobs_finished_total{kind=\"catalog_sync\",outcome=\"succeeded\"} 1",
    )
    .await;

    cancel.cancel();
    timer.await.unwrap();
    scheduler.shutdown().await;
    assert_eq!(job.ran.load(Ordering::SeqCst), 1);
}

#[test]
fn test_reschedule_steps_from_the_due_instant_when_on_time() {
    let base = tokio::time::Instant::now();
    assert_eq!(
        super::timer::reschedule(base, base, Duration::from_mins(1)),
        base + Duration::from_mins(1)
    );
}

#[test]
fn test_reschedule_steps_from_the_wake_instant_when_it_woke_late() {
    let base = tokio::time::Instant::now();
    let woke = base + Duration::from_secs(200);
    assert_eq!(
        super::timer::reschedule(base, woke, Duration::from_mins(1)),
        woke + Duration::from_mins(1)
    );
}

#[tokio::test]
async fn test_no_schedules_still_runs_history_cleanup() {
    let (_dir, app) = scheduled_app(Arc::new(StubDriver::new(0, Ok(RefreshSweep::default()))));
    for _ in 0..17 {
        let id = start_corruptible_attempt(&app.meta);
        app.meta.finish_job_run(&id, JobOutcome::succeeded(100, 0, 0)).unwrap();
    }
    let scheduler = Arc::new(JobScheduler::new(app.serving.clone(), JobLimits::node_local()));
    let cancel = CancellationToken::new();
    let timer = tokio::spawn(run_schedules(
        app.clone(),
        scheduler.clone(),
        Vec::new(),
        cancel.clone(),
    ));
    await_metric(
        &scheduler,
        "peryx_jobs_finished_total{kind=\"job_history_cleanup\",outcome=\"succeeded\"} 1",
    )
    .await;

    cancel.cancel();
    timer.await.unwrap();
    assert_eq!(job_runs(&app.meta).len(), 16);
    scheduler.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn test_a_schedule_fires_its_job_when_the_interval_elapses() {
    let driver = Arc::new(StubDriver::new(1, Ok(RefreshSweep { checked: 2, changed: 1 })));
    let refresh_started = driver.refresh_started.clone();
    let refresh_calls = driver.refresh_calls.clone();
    let (_dir, app) = scheduled_app(driver);
    let scheduler = Arc::new(JobScheduler::new(app.serving.clone(), JobLimits::node_local()));
    let cancel = CancellationToken::new();
    tokio::spawn(
        run_schedules(app, scheduler.clone(), cache_schedule(60), cancel.clone()).with_subscriber(test_subscriber()),
    );
    tokio::task::yield_now().await;

    tokio::time::advance(Duration::from_mins(1)).await;
    refresh_started.notified().await;

    assert_eq!(refresh_calls.load(Ordering::SeqCst), 1);
    cancel.cancel();
    scheduler.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn test_a_schedule_fires_again_on_each_interval() {
    let driver = Arc::new(StubDriver::new(1, Ok(RefreshSweep { checked: 2, changed: 1 })));
    let refresh_started = driver.refresh_started.clone();
    let refresh_calls = driver.refresh_calls.clone();
    let (_dir, app) = scheduled_app(driver);
    let scheduler = Arc::new(JobScheduler::new(app.serving.clone(), JobLimits::node_local()));
    let cancel = CancellationToken::new();
    tokio::spawn(run_schedules(
        app,
        scheduler.clone(),
        cache_schedule(60),
        cancel.clone(),
    ));
    tokio::task::yield_now().await;

    tokio::time::advance(Duration::from_mins(1)).await;
    refresh_started.notified().await;
    tokio::time::advance(Duration::from_mins(1)).await;
    refresh_started.notified().await;

    assert_eq!(refresh_calls.load(Ordering::SeqCst), 2);
    cancel.cancel();
    scheduler.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn test_a_schedule_that_wakes_late_does_not_replay_missed_runs() {
    let driver = Arc::new(StubDriver::new(1, Ok(RefreshSweep { checked: 2, changed: 1 })));
    let refresh_started = driver.refresh_started.clone();
    let refresh_calls = driver.refresh_calls.clone();
    let (_dir, app) = scheduled_app(driver);
    let scheduler = Arc::new(JobScheduler::new(app.serving.clone(), JobLimits::node_local()));
    let cancel = CancellationToken::new();
    tokio::spawn(run_schedules(
        app,
        scheduler.clone(),
        cache_schedule(60),
        cancel.clone(),
    ));
    tokio::task::yield_now().await;

    // Jump past three intervals in one step: the misfire policy collapses the missed runs into a
    // single fire rather than replaying a backlog.
    tokio::time::advance(Duration::from_secs(200)).await;
    refresh_started.notified().await;
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }

    assert_eq!(refresh_calls.load(Ordering::SeqCst), 1);
    cancel.cancel();
    scheduler.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn test_a_tick_overlapping_a_running_job_is_skipped() {
    let hold = Arc::new(Notify::new());
    let driver = Arc::new(StubDriver::holding(
        1,
        Ok(RefreshSweep { checked: 2, changed: 1 }),
        hold.clone(),
    ));
    let refresh_started = driver.refresh_started.clone();
    let refresh_calls = driver.refresh_calls.clone();
    let (_dir, app) = scheduled_app(driver);
    let scheduler = Arc::new(JobScheduler::new(app.serving.clone(), JobLimits::node_local()));
    let cancel = CancellationToken::new();
    tokio::spawn(run_schedules(
        app,
        scheduler.clone(),
        cache_schedule(60),
        cancel.clone(),
    ));
    tokio::task::yield_now().await;

    tokio::time::advance(Duration::from_mins(1)).await;
    refresh_started.notified().await;
    tokio::time::advance(Duration::from_mins(1)).await;
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }

    assert_eq!(
        refresh_calls.load(Ordering::SeqCst),
        1,
        "the second tick conflicts with the still-running first run and is dropped"
    );
    hold.notify_one();
    cancel.cancel();
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_the_timer_stops_when_cancelled() {
    let (_dir, app) = scheduled_app(Arc::new(StubDriver::new(0, Ok(RefreshSweep::default()))));
    let scheduler = Arc::new(JobScheduler::new(app.serving.clone(), JobLimits::node_local()));
    let cancel = CancellationToken::new();
    let timer = tokio::spawn(run_schedules(
        app,
        scheduler.clone(),
        cache_schedule(60),
        cancel.clone(),
    ));

    cancel.cancel();
    timer.await.unwrap();
    scheduler.shutdown().await;
}

struct CountedDocs(usize);

impl PackageIndexer for CountedDocs {
    fn documents(&self, _ctx: &IndexerCtx<'_>) -> Result<Vec<PackageDocument>, SearchError> {
        Ok((0..self.0)
            .map(|serial| PackageDocument {
                display_name: format!("pkg{serial}"),
                normalized_name: format!("pkg{serial}"),
                route: "root".to_owned(),
                index: "root".to_owned(),
                ecosystem: "alpha".to_owned(),
                source: PackageSource::Cached,
                available_locally: false,
                summary: None,
                text: format!("pkg{serial}"),
            })
            .collect())
    }
}

struct FailingDocs;

impl PackageIndexer for FailingDocs {
    fn documents(&self, _ctx: &IndexerCtx<'_>) -> Result<Vec<PackageDocument>, SearchError> {
        Err(SearchError::Indexer("stored record could not be indexed".to_owned()))
    }
}

fn state_with_indexer(indexer: Arc<dyn PackageIndexer>) -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let clock: Clock = Arc::new(|| 1_000);
    let mut state = AppState::with_clock(meta, blobs, 60, Vec::new(), clock);
    let driver = Arc::new(StubDriver::new(0, Ok(RefreshSweep { checked: 0, changed: 0 })));
    state.register_ecosystem(driver, indexer);
    (dir, state)
}

fn rebuild_job(chunk: usize) -> Arc<SearchRebuildJob> {
    Arc::new(SearchRebuildJob::new(NonZeroUsize::new(chunk).unwrap()))
}

#[tokio::test]
async fn test_search_rebuild_persists_a_node_wide_run_and_reports_documents() {
    let (_dir, state) = state_with_indexer(Arc::new(CountedDocs(2)));
    let scheduler = JobScheduler::new(state.serving.clone(), JobLimits::node_local());

    let report = scheduler.run(rebuild_job(1)).await.unwrap();
    scheduler.shutdown().await;

    assert_eq!(
        report,
        JobReport {
            processed: 2,
            changed: 2
        }
    );
    let runs = job_runs(&state.serving.meta);
    assert_eq!(runs.len(), 1);
    let run = &runs[0];
    assert_eq!(
        (run.kind, run.scope.as_str(), run.state, run.items_processed),
        (JobKind::SearchRebuild, "", JobState::Succeeded, 2)
    );
}

#[tokio::test]
async fn test_search_rebuild_cancelled_reports_no_change() {
    let (_dir, state) = state_with_indexer(Arc::new(CountedDocs(2)));
    let cancel = CancellationToken::new();
    cancel.cancel();

    let report = SearchRebuildJob::new(NonZeroUsize::new(1).unwrap())
        .run(&context(state.serving.clone(), cancel))
        .await
        .unwrap();

    assert_eq!(report, JobReport::default());
}

#[tokio::test]
async fn test_search_rebuild_surfaces_an_indexer_failure() {
    let (_dir, state) = state_with_indexer(Arc::new(FailingDocs));

    let failure = SearchRebuildJob::new(NonZeroUsize::new(1).unwrap())
        .run(&context(state.serving.clone(), CancellationToken::new()))
        .await
        .unwrap_err();

    assert_eq!(failure.code(), "search_rebuild");
}

/// An ownership group double whose committed epoch a test advances, to drive the fence deterministically.
struct MutableEpoch(Arc<std::sync::atomic::AtomicU64>);

#[async_trait]
impl crate::state::OwnershipAuthority for MutableEpoch {
    async fn has_home(&self, _authority: &str) -> bool {
        true
    }

    async fn claim_home(&self, _authority: &str) -> Result<crate::state::HomeClaim, crate::state::OwnershipError> {
        Ok(crate::state::HomeClaim::AlreadyHomed)
    }

    fn cluster_status(&self) -> crate::state::ClusterStatus {
        crate::state::ClusterStatus {
            leader: None,
            term: 0,
            voters: Vec::new(),
        }
    }

    async fn committed_epoch(&self, _authority: &str) -> u64 {
        self.0.load(Ordering::SeqCst)
    }

    async fn admit_epoch(&self, _authority: &str, presented: u64) -> bool {
        let current = self.0.load(Ordering::SeqCst);
        current != 0 && presented == current
    }

    async fn transfer_home(
        &self,
        _authority: &str,
        _new_home: &str,
    ) -> Result<Option<crate::state::TransferOutcome>, crate::state::OwnershipError> {
        Ok(None)
    }
}

/// A repository job that records the epoch it leased and then advances its authority's epoch while it
/// runs, superseding the epoch it leased.
struct AdvancingJob {
    epoch: Arc<std::sync::atomic::AtomicU64>,
    leased: Arc<std::sync::atomic::AtomicU64>,
}

#[async_trait]
impl NodeJob for AdvancingJob {
    fn kind(&self) -> &'static str {
        "advancing"
    }

    fn scope(&self) -> &'static str {
        "proj"
    }

    fn repository(&self) -> Option<&str> {
        Some("proj")
    }

    fn persist_as(&self) -> Option<JobKind> {
        Some(JobKind::CatalogSync)
    }

    async fn run(&self, ctx: &JobContext) -> Result<JobReport, JobFailure> {
        self.leased.store(ctx.authority_fence(), Ordering::SeqCst);
        self.epoch.fetch_add(1, Ordering::SeqCst);
        Ok(JobReport::default())
    }
}

#[tokio::test]
async fn test_a_run_whose_authority_advances_mid_run_is_fenced() {
    let (_dir, state) = serving();
    let epoch = Arc::new(std::sync::atomic::AtomicU64::new(5));
    let leased = Arc::new(std::sync::atomic::AtomicU64::new(0));
    state.set_ownership_authority(Arc::new(MutableEpoch(epoch.clone())));
    let scheduler = JobScheduler::new(state.clone(), limits(2, 4, 2, 2));

    // The job leases epoch 5, then advances the authority to 6 while it runs, so its success is fenced.
    scheduler.submit(Arc::new(AdvancingJob {
        epoch,
        leased: leased.clone(),
    }));
    await_metric(
        &scheduler,
        "peryx_jobs_finished_total{kind=\"advancing\",outcome=\"failed\"} 1",
    )
    .await;

    assert_eq!(
        leased.load(Ordering::SeqCst),
        5,
        "the run leased the committed epoch as its fence"
    );
    let runs = job_runs(&state.meta);
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].error.as_deref(),
        Some("authority_fenced: a newer authority epoch superseded this run"),
    );
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_admit_authority_epoch_admits_all_work_without_a_group() {
    let (_dir, state) = serving();

    // A process running no consensus group has no authority to supersede its work, so it admits any
    // epoch, including the closed sentinel.
    assert!(state.admit_authority_epoch("proj", 7).await);
    assert!(state.admit_authority_epoch("proj", 0).await);
}

#[tokio::test]
async fn test_transfer_authority_home_delegates_to_the_group() {
    let (_dir, state) = serving();

    // A process running no consensus group cannot commit a transfer, so the delegate reports nothing moved.
    assert_eq!(state.transfer_authority_home("proj", "west").await.unwrap(), None);

    // With a group registered, the delegate forwards to it; this one commits nothing itself.
    state.set_ownership_authority(Arc::new(MutableEpoch(Arc::new(std::sync::atomic::AtomicU64::new(5)))));
    assert_eq!(state.transfer_authority_home("proj", "west").await.unwrap(), None);
}

#[tokio::test]
async fn test_a_run_under_the_current_epoch_is_not_fenced() {
    let (_dir, state) = serving();
    state.set_ownership_authority(Arc::new(MutableEpoch(Arc::new(std::sync::atomic::AtomicU64::new(5)))));
    let scheduler = JobScheduler::new(state.clone(), limits(2, 4, 2, 2));

    // A run that leaves its authority's epoch unchanged keeps the epoch it leased, so it is admitted.
    scheduler.submit(TestJob::persisting_repository(
        "steady",
        "proj",
        "proj",
        Action::Return(Ok(JobReport::default())),
    ));
    await_metric(
        &scheduler,
        "peryx_jobs_finished_total{kind=\"steady\",outcome=\"succeeded\"} 1",
    )
    .await;
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_write_ledger_reap_drains_settled_rows_and_keeps_pending() {
    let (_dir, state) = serving();
    // The serving clock is fixed at 1000, so settle the reapable rows far enough in the past that the
    // retention window has elapsed by then. An admitted intent and a finalized operation are reaped; a
    // pending intent whose write may still finalize is kept.
    let past = -3000;
    let limits = peryx_storage::meta::IntentLimits {
        max_records: 1000,
        max_bytes: 1 << 20,
        backpressure_percent: 80,
    };
    state
        .meta
        .stage_intent(
            peryx_storage::meta::IntentAdmission {
                authority: "auth",
                key: "done",
                digest: "digest-a",
                size: 10,
                payload: b"x",
            },
            limits,
            past,
        )
        .unwrap();
    state
        .meta
        .advance_intent("done", peryx_storage::meta::IntentPhase::Admitted, past)
        .unwrap();
    state
        .meta
        .stage_intent(
            peryx_storage::meta::IntentAdmission {
                authority: "auth",
                key: "live",
                digest: "digest-b",
                size: 10,
                payload: b"x",
            },
            limits,
            past,
        )
        .unwrap();
    state.meta.claim_operation("op", Some(0), past).unwrap();
    state
        .meta
        .finalize_operation("op", peryx_storage::meta::OperationResult::Published, b"body", past)
        .unwrap();

    let report = super::WriteLedgerReap
        .run(&context(state.clone(), CancellationToken::new()))
        .await
        .unwrap();

    assert_eq!(
        report.processed, 2,
        "one admitted intent and one finalized operation reaped"
    );
    assert_eq!(state.meta.staged_intent("done").unwrap(), None);
    assert_eq!(
        state.meta.staged_intent("live").unwrap().unwrap().phase,
        peryx_storage::meta::IntentPhase::Pending
    );
    assert_eq!(state.meta.operation_outcome("op").unwrap(), None);
    assert_eq!(super::WriteLedgerReap.kind(), "write_ledger_reap");
    assert_eq!(super::WriteLedgerReap.scope(), "");
}

#[tokio::test]
async fn test_write_ledger_reap_stops_when_cancelled() {
    let (_dir, state) = serving();
    let cancel = CancellationToken::new();
    cancel.cancel();

    let report = super::WriteLedgerReap.run(&context(state, cancel)).await.unwrap();

    assert_eq!(report, JobReport::default());
}

#[tokio::test]
async fn test_write_ledger_reap_surfaces_a_storage_fault() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reap.redb");
    MetaStore::open(&path).unwrap();
    // A read-only store refuses the prune's write, so the reap surfaces the fault as a job failure
    // rather than reporting a clean sweep it never performed.
    let meta = MetaStore::open_existing_read_only(&path).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let clock: Clock = Arc::new(|| 1_000);
    let state = AppState::with_clock(meta, blobs, 60, Vec::new(), clock).serving;

    let failure = super::WriteLedgerReap
        .run(&context(state, CancellationToken::new()))
        .await
        .unwrap_err();
    assert!(failure.message().contains("read-only"), "{}", failure.message());
}

/// An ownership group double reporting a fixed cluster term, so a test mints a chosen fencing epoch for
/// a cluster-singleton lease.
struct FixedTerm(u64);

#[async_trait]
impl crate::state::OwnershipAuthority for FixedTerm {
    async fn has_home(&self, _authority: &str) -> bool {
        true
    }

    async fn claim_home(&self, _authority: &str) -> Result<crate::state::HomeClaim, crate::state::OwnershipError> {
        Ok(crate::state::HomeClaim::AlreadyHomed)
    }

    fn cluster_status(&self) -> crate::state::ClusterStatus {
        crate::state::ClusterStatus {
            leader: None,
            term: self.0,
            voters: Vec::new(),
        }
    }

    async fn committed_epoch(&self, _authority: &str) -> u64 {
        0
    }

    async fn admit_epoch(&self, _authority: &str, _presented: u64) -> bool {
        false
    }

    async fn transfer_home(
        &self,
        _authority: &str,
        _new_home: &str,
    ) -> Result<Option<crate::state::TransferOutcome>, crate::state::OwnershipError> {
        Ok(None)
    }
}

/// A cluster-singleton job that records the fence it leased and, when asked, claims the lease under a
/// higher epoch mid-run to stand in for another node superseding it.
struct SingletonJob {
    key: String,
    leased: Arc<AtomicU64>,
    ran: Arc<AtomicUsize>,
    supersede: Option<(String, u64)>,
}

impl SingletonJob {
    fn new(key: &str, leased: Arc<AtomicU64>, ran: Arc<AtomicUsize>) -> Arc<Self> {
        Arc::new(Self {
            key: key.to_owned(),
            leased,
            ran,
            supersede: None,
        })
    }

    fn superseding(key: &str, leased: Arc<AtomicU64>, ran: Arc<AtomicUsize>, holder: &str, epoch: u64) -> Arc<Self> {
        Arc::new(Self {
            key: key.to_owned(),
            leased,
            ran,
            supersede: Some((holder.to_owned(), epoch)),
        })
    }
}

#[async_trait]
impl NodeJob for SingletonJob {
    fn kind(&self) -> &'static str {
        "singleton"
    }

    fn scope(&self) -> &'static str {
        ""
    }

    fn lease_scope(&self) -> LeaseScope {
        LeaseScope::ClusterSingleton(self.key.clone())
    }

    fn persist_as(&self) -> Option<JobKind> {
        Some(JobKind::CatalogSync)
    }

    async fn run(&self, ctx: &JobContext) -> Result<JobReport, JobFailure> {
        self.ran.fetch_add(1, Ordering::SeqCst);
        self.leased.store(ctx.authority_fence(), Ordering::SeqCst);
        if let Some((holder, epoch)) = &self.supersede {
            let now = (ctx.state().clock)();
            ctx.state()
                .meta
                .claim_job_lease(&self.key, holder, *epoch, now, 300)
                .expect("a higher epoch takes the lease");
        }
        Ok(JobReport::default())
    }
}

#[tokio::test]
async fn test_a_cluster_singleton_leases_the_cluster_term_and_releases() {
    let (_dir, state) = serving();
    state.set_ownership_authority(Arc::new(FixedTerm(7)));
    let scheduler = JobScheduler::new(state.clone(), limits(2, 4, 2, 2));
    let leased = Arc::new(AtomicU64::new(0));
    let ran = Arc::new(AtomicUsize::new(0));

    scheduler.submit(SingletonJob::new("reclaim", leased.clone(), ran.clone()));
    await_metric(
        &scheduler,
        "peryx_jobs_finished_total{kind=\"singleton\",outcome=\"succeeded\"} 1",
    )
    .await;

    assert_eq!(ran.load(Ordering::SeqCst), 1, "the singleton ran once");
    assert_eq!(
        leased.load(Ordering::SeqCst),
        7,
        "the run leased the cluster term as its fence"
    );
    let lease = state.meta.job_lease("reclaim").unwrap().expect("the lease persisted");
    assert_eq!(lease.epoch, 7);
    assert_eq!(
        lease.state,
        LeaseState::Released,
        "the run released the lease when it finished"
    );
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_a_cluster_singleton_behind_a_higher_term_is_fenced_before_it_runs() {
    let (_dir, state) = serving();
    // Another node already holds the key under term 9; this node's group is behind at term 5.
    state
        .meta
        .claim_job_lease("reclaim", "node-other", 9, 1_000, 300)
        .unwrap();
    state.set_ownership_authority(Arc::new(FixedTerm(5)));
    let scheduler = JobScheduler::new(state.clone(), limits(2, 4, 2, 2));
    let leased = Arc::new(AtomicU64::new(0));
    let ran = Arc::new(AtomicUsize::new(0));

    scheduler.submit(SingletonJob::new("reclaim", leased.clone(), ran.clone()));
    await_metric(
        &scheduler,
        "peryx_jobs_finished_total{kind=\"singleton\",outcome=\"failed\"} 1",
    )
    .await;

    assert_eq!(ran.load(Ordering::SeqCst), 0, "a fenced run never executes its work");
    let runs = job_runs(&state.meta);
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].error.as_deref(),
        Some("lease_not_held: a newer fence 9 supersedes the applied fence 5"),
    );
    // The higher holder keeps the lease untouched.
    let lease = state.meta.job_lease("reclaim").unwrap().unwrap();
    assert_eq!(lease.holder, "node-other");
    assert_eq!(lease.epoch, 9);
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_a_cluster_singleton_superseded_mid_run_is_fenced() {
    let (_dir, state) = serving();
    state.set_ownership_authority(Arc::new(FixedTerm(7)));
    let scheduler = JobScheduler::new(state.clone(), limits(2, 4, 2, 2));
    let leased = Arc::new(AtomicU64::new(0));
    let ran = Arc::new(AtomicUsize::new(0));

    // The run leases term 7, then another node takes the key at term 12 while it runs, so its success is
    // fenced rather than counted.
    scheduler.submit(SingletonJob::superseding(
        "reclaim",
        leased.clone(),
        ran.clone(),
        "node-other",
        12,
    ));
    await_metric(
        &scheduler,
        "peryx_jobs_finished_total{kind=\"singleton\",outcome=\"failed\"} 1",
    )
    .await;

    assert_eq!(ran.load(Ordering::SeqCst), 1, "the run executed before it was fenced");
    assert_eq!(leased.load(Ordering::SeqCst), 7, "the run leased term 7");
    let runs = job_runs(&state.meta);
    assert_eq!(
        runs[0].error.as_deref(),
        Some("authority_fenced: a newer holder superseded this cluster-singleton run"),
    );
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_a_node_local_job_takes_no_lease() {
    let (_dir, state) = serving();
    state.set_ownership_authority(Arc::new(FixedTerm(4)));
    let scheduler = JobScheduler::new(state.clone(), limits(2, 4, 2, 2));

    // A node-wide node-local job runs on every node, so it claims no control-plane lease.
    scheduler.submit(TestJob::persisting(
        "cleanup",
        "",
        Action::Return(Ok(JobReport::default())),
    ));
    await_metric(
        &scheduler,
        "peryx_jobs_finished_total{kind=\"cleanup\",outcome=\"succeeded\"} 1",
    )
    .await;

    assert!(
        state.meta.job_leases().unwrap().is_empty(),
        "a node-local job records no lease"
    );
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_a_restarted_node_leases_the_live_term_not_a_persisted_token() {
    let (_dir, state) = serving();
    // A prior process held and released the key at term 5; the group has since advanced to term 6.
    state
        .meta
        .claim_job_lease("reclaim", "node-old", 5, 1_000, 300)
        .unwrap();
    state.meta.release_job_lease("reclaim", "node-old", 5).unwrap();
    state.set_ownership_authority(Arc::new(FixedTerm(6)));
    let scheduler = JobScheduler::new(state.clone(), limits(2, 4, 2, 2));
    let leased = Arc::new(AtomicU64::new(0));
    let ran = Arc::new(AtomicUsize::new(0));

    scheduler.submit(SingletonJob::new("reclaim", leased.clone(), ran.clone()));
    await_metric(
        &scheduler,
        "peryx_jobs_finished_total{kind=\"singleton\",outcome=\"succeeded\"} 1",
    )
    .await;

    assert_eq!(
        leased.load(Ordering::SeqCst),
        6,
        "the run minted the live term, not the persisted token 5"
    );
    assert_eq!(state.meta.job_lease("reclaim").unwrap().unwrap().epoch, 6);
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_a_cluster_singleton_without_a_group_leases_the_zero_sentinel() {
    let (_dir, state) = serving();
    let scheduler = JobScheduler::new(state.clone(), limits(2, 4, 2, 2));
    let leased = Arc::new(AtomicU64::new(9));
    let ran = Arc::new(AtomicUsize::new(0));

    // A lone node runs no group, so it leases every singleton under the closed 0 term without contention.
    scheduler.submit(SingletonJob::new("reclaim", leased.clone(), ran.clone()));
    await_metric(
        &scheduler,
        "peryx_jobs_finished_total{kind=\"singleton\",outcome=\"succeeded\"} 1",
    )
    .await;

    assert_eq!(ran.load(Ordering::SeqCst), 1);
    assert_eq!(leased.load(Ordering::SeqCst), 0, "no group leases the 0 sentinel");
    let lease = state.meta.job_lease("reclaim").unwrap().unwrap();
    assert_eq!(lease.epoch, 0);
    assert_eq!(lease.state, LeaseState::Released);
    scheduler.shutdown().await;
}
