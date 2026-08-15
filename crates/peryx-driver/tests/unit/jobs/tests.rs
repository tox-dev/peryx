use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use peryx_core::Ecosystem;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::{FinishJobRun, JobKind, JobOutcome, JobRunQuery, JobState, LeaseState, MetaStore, NewJobRun};
use rstest::rstest;
use tokio::sync::{Notify, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::instrument::WithSubscriber as _;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt as _;

use super::attempts::JobAttemptError;
use super::scheduler::{JobLimits, Submit};
use super::{
    CacheRefreshJob, CancelJobRun, IdleReclaimJob, IntentFinalizeJob, JobContext, JobFailure, JobHistoryCleanup,
    JobReport, JobScheduler, LeaseScope, NodeJob, NodeJobMetadata, PluginScheduledJob, RegisteredScheduledJob,
    Schedule, ScheduledJob, ScheduledJobFactory, SearchRebuildJob, run_schedules, scheduled_job, submit_maintenance,
};
use crate::serving::{CacheRefresher, IdleReclaimer, IntentFinalizer, RefreshSweep};
use crate::state::{AppState, Clock, ServingState};
use peryx_search::{ContentSource, IndexerCtx, SearchDocument, SearchDocumentProvider, SearchError};

fn serving() -> (tempfile::TempDir, Arc<ServingState>) {
    serving_with(peryx_ha::AvailabilityCapabilities::default())
}

fn serving_with(capabilities: peryx_ha::AvailabilityCapabilities) -> (tempfile::TempDir, Arc<ServingState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let clock: Clock = Arc::new(|| 1_000);
    let mut state = AppState::with_clock(meta, blobs, 60, Vec::new(), clock);
    install_distributed(&mut state, capabilities);
    (dir, state.serving)
}

fn install_distributed(state: &mut AppState, capabilities: peryx_ha::AvailabilityCapabilities) {
    state
        .install_distributed_availability(peryx_ha::AvailabilityStateInstall {
            role: peryx_core::NodeRole::Writer,
            topology: peryx_core::TopologyConfig::default(),
            blobs: peryx_ha::BlobServices::new(None, Arc::new(UnavailableDurability)),
            analytics: Arc::new(UnavailableCompleteness),
            capabilities,
            authority_drainer: None,
            operations: None,
        })
        .unwrap();
}

fn serving_with_authority(authority: Arc<dyn peryx_ha::OwnershipAuthority>) -> (tempfile::TempDir, Arc<ServingState>) {
    serving_with(peryx_ha::AvailabilityCapabilities {
        ownership: Some(authority),
        ..Default::default()
    })
}

struct UnavailableDurability;

#[async_trait]
impl peryx_ha::BlobWriteDurability for UnavailableDurability {
    async fn confirm(&self, _write: peryx_ha::CommittedBlob<'_>) -> peryx_ha::WriteDurability {
        peryx_ha::WriteDurability::Unavailable
    }
}

struct UnavailableCompleteness;

impl peryx_ha::AnalyticsCompleteness for UnavailableCompleteness {
    fn assess(
        &self,
        _meta: &dyn peryx_ha::AnalyticsSnapshotStore,
        _expected: &[peryx_ha::ExpectedProducer],
        _query: &peryx_ha::CompletenessQuery,
    ) -> Result<peryx_ha::CompletenessReport, peryx_ha::CompletenessError> {
        Err(peryx_ha::CompletenessError)
    }
}

#[tokio::test]
async fn test_distributed_blob_confirmation_uses_the_installed_durability() {
    let (_dir, state) = serving();
    let digest = peryx_ha::Digest::of(b"blob");

    assert_eq!(
        state
            .confirm_blob_write(peryx_ha::CommittedBlob::new(
                &digest,
                "repository",
                peryx_ha::AuthorityEpoch(7),
                None,
                peryx_ha::BlobDurability::Filesystem,
            ))
            .await,
        peryx_ha::WriteDurability::Unavailable
    );
}

#[test]
fn test_distributed_analytics_uses_the_installed_completeness_reader() {
    let (_dir, state) = serving();

    assert_eq!(
        state.analytics_completeness().unwrap().assess(
            &state.meta,
            &[],
            &peryx_ha::CompletenessQuery {
                from_day: 1,
                to_day: 2,
                today: 2,
                repository: None,
            },
        ),
        Err(peryx_ha::CompletenessError)
    );
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

enum Action {
    Return(Result<JobReport, String>),
    Block(Arc<Notify>),
    UntilCancelled,
    FailWhenCancelled,
    IgnoreCancellation,
    FinishExternally,
    Panic,
}

struct TestJob {
    kind: &'static str,
    scope: String,
    repository: Option<String>,
    persist: Option<JobKind>,
    action: Action,
    started: Arc<Notify>,
    finished: Arc<Notify>,
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
            finished: Arc::new(Notify::new()),
            ran: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn persisting(kind: &'static str, scope: &str, action: Action) -> Arc<Self> {
        Arc::new(Self {
            kind,
            scope: scope.to_owned(),
            repository: None,
            persist: Some(JobKind::new("cache_refresh").unwrap()),
            action,
            started: Arc::new(Notify::new()),
            finished: Arc::new(Notify::new()),
            ran: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn persisting_repository(kind: &'static str, scope: &str, repository: &str, action: Action) -> Arc<Self> {
        Arc::new(Self {
            kind,
            scope: scope.to_owned(),
            repository: Some(repository.to_owned()),
            persist: Some(JobKind::new("plugin_sync").unwrap()),
            action,
            started: Arc::new(Notify::new()),
            finished: Arc::new(Notify::new()),
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

    fn metadata(&self) -> NodeJobMetadata<'_> {
        NodeJobMetadata {
            lease_scope: LeaseScope::NodeLocal,
            repository: self.repository.as_deref(),
            persist_as: self.persist.clone(),
        }
    }

    async fn run(&self, ctx: &JobContext) -> Result<JobReport, JobFailure> {
        self.ran.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        let result = match &self.action {
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
            Action::IgnoreCancellation => std::future::pending().await,
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
        };
        self.finished.notify_one();
        result
    }
}

struct StubDriver {
    reclaim: usize,
    finalize: u64,
    refresh: Result<RefreshSweep, String>,
    reclaim_calls: Arc<AtomicUsize>,
    finalize_calls: Arc<AtomicUsize>,
    refresh_calls: Arc<AtomicUsize>,
    refresh_started: Arc<Notify>,
    hold: Option<Arc<Notify>>,
}

impl StubDriver {
    fn new(reclaim: usize, refresh: Result<RefreshSweep, String>) -> Self {
        Self {
            reclaim,
            finalize: 3,
            refresh,
            reclaim_calls: Arc::new(AtomicUsize::new(0)),
            finalize_calls: Arc::new(AtomicUsize::new(0)),
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
}

#[async_trait]
impl IdleReclaimer for StubDriver {
    async fn reclaim_idle(&self, _state: Arc<ServingState>) -> usize {
        self.reclaim_calls.fetch_add(1, Ordering::SeqCst);
        self.reclaim
    }
}

#[async_trait]
impl IntentFinalizer for StubDriver {
    async fn finalize_admitted(&self, _state: Arc<ServingState>) -> u64 {
        self.finalize_calls.fetch_add(1, Ordering::SeqCst);
        self.finalize
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

fn test_subscriber() -> impl tracing::Subscriber + Send + Sync {
    tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::TRACE)
        .finish()
}

struct EventTarget(Mutex<Option<oneshot::Sender<&'static str>>>);

impl<Subscriber> tracing_subscriber::Layer<Subscriber> for EventTarget
where
    Subscriber: tracing::Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _context: tracing_subscriber::layer::Context<'_, Subscriber>) {
        self.0
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .send(event.metadata().target())
            .unwrap();
    }
}

#[tokio::test]
async fn test_a_succeeding_job_runs_and_is_not_recorded_without_persistence() {
    let (_dir, state) = serving();
    let scheduler = Arc::new(JobScheduler::new(state.clone(), limits(2, 4, 2, 2)));
    let job = TestJob::new(
        "probe",
        "a",
        Action::Return(Ok(JobReport {
            processed: 4,
            changed: 2,
        })),
    );
    assert_eq!(
        scheduler.run(job.clone()).await.unwrap(),
        JobReport {
            processed: 4,
            changed: 2
        }
    );
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
    let scheduler = Arc::new(JobScheduler::new(state.clone(), limits(2, 4, 2, 2)));
    let selected = TestJob::persisting("probe", "selected", Action::UntilCancelled);
    let other = TestJob::persisting("probe", "other", Action::UntilCancelled);
    let selected_run = tokio::spawn({
        let scheduler = scheduler.clone();
        let selected = selected.clone();
        async move { scheduler.run(selected).await }
    });
    let other_run = tokio::spawn({
        let scheduler = scheduler.clone();
        let other = other.clone();
        async move { scheduler.run(other).await }
    });
    selected.started.notified().await;
    other.started.notified().await;
    let selected_id = job_runs(&state.meta)
        .into_iter()
        .find(|run| run.scope == "selected")
        .unwrap()
        .id;

    assert_eq!(scheduler.cancel_job_run(&selected_id).unwrap(), CancelJobRun::Requested);
    assert_eq!(selected_run.await.unwrap().unwrap(), JobReport::default());
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
    assert_eq!(other_run.await.unwrap().unwrap(), JobReport::default());
}

#[tokio::test]
async fn test_cancel_job_run_records_cancelled_when_the_job_returns_an_error() {
    let (_dir, state) = serving();
    let scheduler = Arc::new(JobScheduler::new(state.clone(), limits(1, 2, 1, 1)));
    let job = TestJob::persisting("probe", "failing", Action::FailWhenCancelled);
    let run = tokio::spawn({
        let scheduler = scheduler.clone();
        let job = job.clone();
        async move { scheduler.run(job).await }
    });
    job.started.notified().await;
    let id = job_runs(&state.meta)[0].id.clone();

    assert_eq!(scheduler.cancel_job_run(&id).unwrap(), CancelJobRun::Requested);
    assert_eq!(run.await.unwrap().unwrap_err(), "test: cancelled at boundary");
    assert_eq!(
        state.meta.get_job_run(&id).unwrap().unwrap(),
        peryx_storage::meta::JobRunRecord {
            id,
            kind: JobKind::new("cache_refresh").unwrap(),
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
            kind: JobKind::new("cache_refresh").unwrap(),
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
            "plugin_sync",
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
    assert_eq!(state.job_attempts.cancel(&run.id).unwrap(), CancelJobRun::Finished);
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
    assert_eq!(state.job_attempts.cancel(&run.id).unwrap(), CancelJobRun::Finished);
    scheduler.shutdown().await;
}

#[test]
fn test_attempt_control_rejects_an_external_finish_and_releases_the_token() {
    let (_dir, state) = serving();
    let id = state
        .job_attempts
        .start(
            NewJobRun {
                kind: JobKind::new("cache_refresh").unwrap(),
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
    assert_eq!(state.job_attempts.cancel(&id).unwrap(), CancelJobRun::Finished);
}

fn start_corruptible_attempt(store: &MetaStore) -> String {
    store
        .start_job_run(NewJobRun {
            kind: JobKind::new("cache_refresh").unwrap(),
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
            kind: JobKind::new("cache_refresh").unwrap(),
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
            kind: JobKind::new("cache_refresh").unwrap(),
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
    let stubborn = TestJob::new("probe", "a", Action::IgnoreCancellation);
    scheduler.submit(stubborn.clone());
    stubborn.started.notified().await;
    let shutdown = tokio::spawn(async move { scheduler.shutdown().await });
    tokio::time::advance(Duration::from_millis(50)).await;
    shutdown.await.unwrap();
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
    scheduler.run(job).await.unwrap();
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

#[test]
fn test_job_context_reads_the_injected_clock() {
    let (_dir, state) = serving();

    assert_eq!(context(state, CancellationToken::new()).now(), 1_000);
}

#[tokio::test]
async fn test_idle_reclaim_reports_changed_resources() {
    let (_dir, state) = serving();
    let driver = Arc::new(StubDriver::new(2, Ok(RefreshSweep::default())));
    let job = IdleReclaimJob {
        ecosystem: Ecosystem::new("example"),
        reclaimer: driver.clone(),
    };

    assert_eq!(
        job.run(&context(state, CancellationToken::new())).await.unwrap(),
        JobReport {
            processed: 2,
            changed: 2
        }
    );
    assert_eq!(driver.reclaim_calls.load(Ordering::SeqCst), 1);
    assert_eq!(job.kind(), "idle_reclaim");
    assert_eq!(job.scope(), "example");
    assert_eq!(job.persist_as(), Some(JobKind::new("idle_reclaim").unwrap()));
}

#[tokio::test]
async fn test_intent_finalize_reports_finalized_intents() {
    let (_dir, state) = serving();
    let driver = Arc::new(StubDriver::new(0, Ok(RefreshSweep::default())));
    let job = IntentFinalizeJob {
        ecosystem: Ecosystem::new("example"),
        finalizer: driver.clone(),
    };

    assert_eq!(
        job.run(&context(state, CancellationToken::new())).await.unwrap(),
        JobReport {
            processed: 3,
            changed: 3
        }
    );
    assert_eq!(driver.finalize_calls.load(Ordering::SeqCst), 1);
    assert_eq!(job.kind(), "intent_finalize");
    assert_eq!(job.scope(), "example");
    assert_eq!(job.persist_as(), Some(JobKind::new("intent_finalize").unwrap()));
}

#[tokio::test]
async fn test_cache_refresh_reports_the_sweep() {
    let (_dir, state) = serving();
    let job = CacheRefreshJob {
        ecosystem: Ecosystem::new("example"),
        refresher: Arc::new(StubDriver::new(0, Ok(RefreshSweep { checked: 3, changed: 1 }))),
    };

    assert_eq!(
        job.run(&context(state, CancellationToken::new())).await.unwrap(),
        JobReport {
            processed: 3,
            changed: 1
        }
    );
    assert_eq!(job.kind(), "cache_refresh");
    assert_eq!(job.scope(), "example");
    assert_eq!(job.persist_as(), Some(JobKind::new("cache_refresh").unwrap()));
}

#[tokio::test]
async fn test_cache_refresh_propagates_failure() {
    let (_dir, state) = serving();
    let job = CacheRefreshJob {
        ecosystem: Ecosystem::new("example"),
        refresher: Arc::new(StubDriver::new(0, Err("upstream down".to_owned()))),
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
async fn test_cancelled_capability_jobs_do_no_work() {
    let (_dir, state) = serving();
    let driver = Arc::new(StubDriver::new(1, Ok(RefreshSweep { checked: 9, changed: 9 })));
    let cancel = CancellationToken::new();
    cancel.cancel();
    for job in [
        Arc::new(IdleReclaimJob {
            ecosystem: Ecosystem::new("example"),
            reclaimer: driver.clone(),
        }) as Arc<dyn NodeJob>,
        Arc::new(IntentFinalizeJob {
            ecosystem: Ecosystem::new("example"),
            finalizer: driver.clone(),
        }),
        Arc::new(CacheRefreshJob {
            ecosystem: Ecosystem::new("example"),
            refresher: driver.clone(),
        }),
    ] {
        assert_eq!(
            job.run(&context(state.clone(), cancel.clone())).await.unwrap(),
            JobReport::default()
        );
    }
    assert_eq!(
        (
            driver.reclaim_calls.load(Ordering::SeqCst),
            driver.finalize_calls.load(Ordering::SeqCst),
            driver.refresh_calls.load(Ordering::SeqCst),
        ),
        (0, 0, 0)
    );
}

#[tokio::test]
async fn test_submit_maintenance_runs_each_registered_capability() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let clock: Clock = Arc::new(|| 1_000);
    let mut state = AppState::with_clock(meta, blobs, 60, Vec::new(), clock);
    let driver = Arc::new(StubDriver::new(1, Ok(RefreshSweep { checked: 2, changed: 1 })));
    let refresh_started = driver.refresh_started.clone();
    state.register_idle_reclaimer(Ecosystem::new("example"), driver.clone());
    state.register_intent_finalizer(Ecosystem::new("example"), driver.clone());
    state.register_cache_refresher(Ecosystem::new("example"), driver);
    let scheduler = JobScheduler::new(state.serving.clone(), JobLimits::node_local());
    submit_maintenance(&state, &scheduler);
    refresh_started.notified().await;
    scheduler.shutdown().await;
    let runs = job_runs(&state.serving.meta);
    assert_eq!(runs.len(), 3);
    assert!(
        runs.iter()
            .all(|run| run.scope == "example" && run.state == JobState::Succeeded)
    );
    assert_eq!(
        runs.iter()
            .map(|run| (run.kind.as_str(), run.items_processed, run.items_changed))
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from([
            ("cache_refresh", 2, 1),
            ("idle_reclaim", 1, 1),
            ("intent_finalize", 3, 3),
        ])
    );
}

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

    fn metadata(&self) -> NodeJobMetadata<'_> {
        NodeJobMetadata {
            lease_scope: LeaseScope::NodeLocal,
            repository: None,
            persist_as: None,
        }
    }

    async fn run(&self, _ctx: &JobContext) -> Result<JobReport, JobFailure> {
        Ok(JobReport::default())
    }
}

#[tokio::test]
async fn test_a_node_local_job_runs_without_a_persisted_record() {
    let (_dir, state) = serving();
    let scheduler = JobScheduler::new(state.clone(), JobLimits::node_local());
    let job = BareJob { scope: String::new() };

    assert_eq!((job.repository(), job.persist_as()), (None, None));
    assert_eq!(scheduler.run(Arc::new(job)).await.unwrap(), JobReport::default());
    assert!(job_runs(&state.meta).is_empty());
    scheduler.shutdown().await;
}

#[test]
fn test_job_failure_exposes_its_stable_category_and_safe_message() {
    let failure = JobFailure::new("retryable_timeout", "catalog request timed out");

    assert_eq!(failure.code(), "retryable_timeout");
    assert_eq!(failure.message(), "catalog request timed out");
    assert_eq!(failure.to_string(), "retryable_timeout: catalog request timed out");
}

#[test]
fn test_job_failure_parts_preserve_code_and_message() {
    assert_eq!(
        JobFailure::new("failed", "safe message").into_parts(),
        ("failed", "safe message".to_owned())
    );
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

    let report = JobHistoryCleanup { retain: 16 }
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

    let report = JobHistoryCleanup { retain: 16 }
        .run(&context(state.clone(), cancel))
        .await
        .unwrap();

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

    let error = JobHistoryCleanup { retain: 16 }
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
    install_distributed(&mut state, peryx_ha::AvailabilityCapabilities::default());
    state.register_idle_reclaimer(Ecosystem::new("example"), driver.clone());
    state.register_intent_finalizer(Ecosystem::new("example"), driver.clone());
    state.register_cache_refresher(Ecosystem::new("example"), driver);
    (dir, Arc::new(state))
}

fn cache_schedule(secs: u64) -> Vec<Schedule> {
    vec![Schedule {
        job: ScheduledJob::CacheMaintenance,
        interval: Duration::from_secs(secs),
    }]
}

struct TestScheduleFactory {
    job: Result<Arc<dyn NodeJob>, String>,
}

impl ScheduledJobFactory for TestScheduleFactory {
    fn kind(&self) -> &'static str {
        "plugin_sync"
    }

    fn settings(&self) -> toml::Table {
        toml::Table::new()
    }

    fn create(&self, _app: &AppState) -> Result<Arc<dyn NodeJob>, String> {
        self.job.clone()
    }
}

fn plugin_schedule(secs: u64, job: Result<Arc<dyn NodeJob>, String>) -> Vec<Schedule> {
    vec![Schedule {
        job: ScheduledJob::Plugin(PluginScheduledJob::new(
            Ecosystem::new("example"),
            Arc::new(TestScheduleFactory { job }),
        )),
        interval: Duration::from_secs(secs),
    }]
}

#[test]
fn test_plugin_schedule_equality_uses_its_public_identity() {
    let left = PluginScheduledJob::new(
        Ecosystem::new("example"),
        Arc::new(TestScheduleFactory {
            job: Ok(TestJob::new(
                "plugin_sync",
                "alpha",
                Action::Return(Ok(JobReport::default())),
            )),
        }),
    );
    let right = PluginScheduledJob::new(
        Ecosystem::new("example"),
        Arc::new(TestScheduleFactory {
            job: Ok(TestJob::new(
                "plugin_sync",
                "beta",
                Action::Return(Ok(JobReport::default())),
            )),
        }),
    );

    assert_eq!(left, right);
}

#[test]
fn test_plugin_schedule_debug_names_its_public_identity() {
    let schedule = PluginScheduledJob::new(
        Ecosystem::new("example"),
        Arc::new(TestScheduleFactory {
            job: Ok(TestJob::new(
                "plugin_sync",
                "alpha",
                Action::Return(Ok(JobReport::default())),
            )),
        }),
    );
    let debug = format!("{schedule:?}");

    assert!(
        ["PluginScheduledJob", "example", "plugin_sync"]
            .into_iter()
            .all(|field| debug.contains(field))
    );
    assert_eq!(schedule.ecosystem(), Ecosystem::new("example"));
}

#[test]
fn test_registered_schedule_exposes_its_public_identity() {
    let (_dir, app) = scheduled_app(Arc::new(StubDriver::new(0, Ok(RefreshSweep::default()))));
    let factory = Arc::new(TestScheduleFactory {
        job: Ok(TestJob::new(
            "plugin_sync",
            "alpha",
            Action::Return(Ok(JobReport::default())),
        )),
    });
    let left = RegisteredScheduledJob::new(factory.clone());
    let right = RegisteredScheduledJob::new(factory);

    assert_eq!(left, right);
    assert_eq!(left.kind(), "plugin_sync");
    assert_eq!(left.settings(), toml::Table::new());
    let scheduled = ScheduledJob::Registered(left.clone());
    assert_eq!(scheduled.as_str(), "plugin_sync");
    assert_eq!(scheduled_job(&app, &scheduled).unwrap().kind(), "plugin_sync");
    assert_eq!(
        format!("{left:?}"),
        "RegisteredScheduledJob { kind: \"plugin_sync\", settings: {}, .. }"
    );
}

#[test]
fn test_scheduled_job_settings_follow_the_selected_factory() {
    let factory = Arc::new(TestScheduleFactory {
        job: Ok(TestJob::new(
            "plugin_sync",
            "alpha",
            Action::Return(Ok(JobReport::default())),
        )),
    });
    let jobs = [
        ScheduledJob::CacheMaintenance,
        ScheduledJob::Plugin(PluginScheduledJob::new(Ecosystem::new("example"), factory.clone())),
        ScheduledJob::Registered(RegisteredScheduledJob::new(factory)),
    ];

    assert!(jobs.into_iter().all(|job| job.settings().is_empty()));
}

#[test]
fn test_cache_maintenance_does_not_compile_as_one_factory() {
    let (_dir, app) = scheduled_app(Arc::new(StubDriver::new(0, Ok(RefreshSweep::default()))));

    assert_eq!(
        scheduled_job(&app, &ScheduledJob::CacheMaintenance).err().unwrap(),
        "cache maintenance expands through installed drivers"
    );
}

#[tokio::test(start_paused = true)]
async fn test_an_unsupported_scheduled_job_is_rejected_without_submission() {
    let (_dir, app) = scheduled_app(Arc::new(StubDriver::new(0, Ok(RefreshSweep::default()))));
    let plan = plugin_schedule(60, Err("plugin job rejected".to_owned()));
    let error = scheduled_job(&app, &plan[0].job).err().unwrap();
    assert_eq!(error, "plugin job rejected");
    let scheduler = Arc::new(JobScheduler::new(app.serving.clone(), JobLimits::node_local()));
    let cancel = CancellationToken::new();
    let (event, rejected) = oneshot::channel();
    let subscriber = tracing_subscriber::registry()
        .with(LevelFilter::ERROR)
        .with(EventTarget(Mutex::new(Some(event))));
    let schedule_loop = run_schedules(app.clone(), scheduler.clone(), plan, cancel.clone()).with_subscriber(subscriber);
    let timer = tokio::spawn(schedule_loop);

    assert_eq!(rejected.await.unwrap(), "peryx_driver::jobs::timer");
    cancel.cancel();
    timer.await.unwrap();
    scheduler.shutdown().await;
    assert!(job_runs(&app.serving.meta).is_empty());
}

#[tokio::test(start_paused = true)]
async fn test_a_supported_scheduled_job_is_submitted() {
    let job = TestJob::new("plugin_sync", "alpha", Action::Return(Ok(JobReport::default())));
    let (_dir, app) = scheduled_app(Arc::new(StubDriver::new(0, Ok(RefreshSweep::default()))));
    let scheduler = Arc::new(JobScheduler::new(app.serving.clone(), JobLimits::node_local()));
    let cancel = CancellationToken::new();
    let timer = start_schedules(
        app.clone(),
        scheduler.clone(),
        plugin_schedule(60, Ok(job.clone())),
        cancel.clone(),
    )
    .await;

    tokio::time::advance(Duration::from_mins(1)).await;
    job.finished.notified().await;

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
        let id = start_corruptible_attempt(&app.serving.meta);
        app.serving
            .meta
            .finish_job_run(&id, JobOutcome::succeeded(100, 0, 0))
            .unwrap();
    }
    let scheduler = Arc::new(JobScheduler::new(app.serving.clone(), JobLimits::node_local()));
    let cancel = CancellationToken::new();
    let timer = start_schedules(app.clone(), scheduler.clone(), Vec::new(), cancel.clone()).await;
    scheduler
        .run(TestJob::new("sentinel", "", Action::Return(Ok(JobReport::default()))))
        .await
        .unwrap();

    cancel.cancel();
    timer.await.unwrap();
    assert_eq!(job_runs(&app.serving.meta).len(), 17);
    scheduler.shutdown().await;
}

#[rstest]
#[case::first_tick(1)]
#[case::second_tick(2)]
#[tokio::test(start_paused = true)]
async fn test_a_schedule_fires_on_each_elapsed_interval(#[case] expected_runs: usize) {
    let driver = Arc::new(StubDriver::new(1, Ok(RefreshSweep { checked: 2, changed: 1 })));
    let refresh_started = driver.refresh_started.clone();
    let refresh_calls = driver.refresh_calls.clone();
    let (_dir, app) = scheduled_app(driver);
    let scheduler = Arc::new(JobScheduler::new(app.serving.clone(), JobLimits::node_local()));
    let cancel = CancellationToken::new();
    let timer = start_schedules(app, scheduler.clone(), cache_schedule(60), cancel.clone()).await;

    for _ in 0..expected_runs {
        tokio::time::advance(Duration::from_mins(1)).await;
        refresh_started.notified().await;
    }

    assert_eq!(refresh_calls.load(Ordering::SeqCst), expected_runs);
    cancel.cancel();
    timer.await.unwrap();
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
    let timer = start_schedules(app, scheduler.clone(), cache_schedule(60), cancel.clone()).await;

    tokio::time::advance(Duration::from_secs(200)).await;
    refresh_started.notified().await;

    assert_eq!(refresh_calls.load(Ordering::SeqCst), 1);
    cancel.cancel();
    timer.await.unwrap();
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
    let timer = start_schedules(app, scheduler.clone(), cache_schedule(60), cancel.clone()).await;

    tokio::time::advance(Duration::from_mins(1)).await;
    refresh_started.notified().await;
    tokio::time::advance(Duration::from_mins(1)).await;

    assert_eq!(
        refresh_calls.load(Ordering::SeqCst),
        1,
        "the second tick conflicts with the still-running first run and is dropped"
    );
    hold.notify_one();
    cancel.cancel();
    timer.await.unwrap();
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

async fn start_schedules(
    app: Arc<AppState>,
    scheduler: Arc<JobScheduler>,
    plan: Vec<Schedule>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let (started, ready) = oneshot::channel();
    let timer = tokio::spawn(async move {
        let timer = super::timer::ScheduleTimer::new(plan);
        started.send(()).unwrap();
        timer.run(&app, &scheduler, cancel).await;
    });
    ready.await.unwrap();
    timer
}

struct CountedDocs(usize);

impl SearchDocumentProvider for CountedDocs {
    fn documents(&self, _ctx: &IndexerCtx<'_>) -> Result<Vec<SearchDocument>, SearchError> {
        Ok((0..self.0)
            .map(|serial| SearchDocument {
                display_label: format!("pkg{serial}"),
                resource_key: format!("pkg{serial}"),
                route: "root".to_owned(),
                index: "root".to_owned(),
                ecosystem: "alpha".to_owned(),
                source: ContentSource::Cached,
                available_locally: false,
                summary: None,
                text: format!("pkg{serial}"),
            })
            .collect())
    }
}

struct FailingDocs;

impl SearchDocumentProvider for FailingDocs {
    fn documents(&self, _ctx: &IndexerCtx<'_>) -> Result<Vec<SearchDocument>, SearchError> {
        Err(SearchError::Indexer("stored record could not be indexed".to_owned()))
    }
}

fn state_with_indexer(indexer: Arc<dyn SearchDocumentProvider>) -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let clock: Clock = Arc::new(|| 1_000);
    let mut state = AppState::with_clock(meta, blobs, 60, Vec::new(), clock);
    Arc::get_mut(&mut state.serving).unwrap().search.add_indexer(indexer);
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
        (run.kind.clone(), run.scope.as_str(), run.state, run.items_processed),
        (JobKind::new("search_rebuild").unwrap(), "", JobState::Succeeded, 2)
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

struct TestAuthority {
    epoch: Arc<AtomicU64>,
    term: u64,
    home: bool,
    claims: Arc<AtomicUsize>,
}

fn test_authority(epoch: Arc<AtomicU64>, term: u64) -> Arc<TestAuthority> {
    Arc::new(TestAuthority {
        epoch,
        term,
        home: true,
        claims: Arc::new(AtomicUsize::new(0)),
    })
}

#[async_trait]
impl crate::state::OwnershipAuthority for TestAuthority {
    async fn has_home(&self, _authority: &str) -> bool {
        self.home
    }

    async fn claim_home(&self, _authority: &str) -> Result<crate::state::HomeClaim, crate::state::OwnershipError> {
        self.claims.fetch_add(1, Ordering::SeqCst);
        Ok(crate::state::HomeClaim::AssignedHere)
    }

    fn cluster_status(&self) -> crate::state::ClusterStatus {
        crate::state::ClusterStatus {
            leader: None,
            term: self.term,
            voters: Vec::new(),
        }
    }

    async fn committed_epoch(&self, _authority: &str) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    async fn admit_epoch(&self, _authority: &str, presented: u64) -> bool {
        let current = self.epoch.load(Ordering::SeqCst);
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

#[rstest]
#[case::unowned(false, 1)]
#[case::homed(true, 0)]
#[tokio::test]
async fn test_first_publish_home_claims_only_unowned_authorities(#[case] home: bool, #[case] expected_claims: usize) {
    let claims = Arc::new(AtomicUsize::new(0));
    let (_dir, state) = serving_with_authority(Arc::new(TestAuthority {
        epoch: Arc::new(AtomicU64::new(0)),
        term: 0,
        home,
        claims: claims.clone(),
    }));

    state.claim_first_publish_home("proj").await;

    assert_eq!(claims.load(Ordering::SeqCst), expected_claims);
}

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

    fn metadata(&self) -> NodeJobMetadata<'_> {
        NodeJobMetadata {
            lease_scope: LeaseScope::NodeLocal,
            repository: Some("proj"),
            persist_as: Some(JobKind::new("plugin_sync").unwrap()),
        }
    }

    async fn run(&self, ctx: &JobContext) -> Result<JobReport, JobFailure> {
        self.leased.store(ctx.authority_fence(), Ordering::SeqCst);
        self.epoch.fetch_add(1, Ordering::SeqCst);
        Ok(JobReport::default())
    }
}

#[tokio::test]
async fn test_a_run_whose_authority_advances_mid_run_is_fenced() {
    let epoch = Arc::new(std::sync::atomic::AtomicU64::new(5));
    let leased = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (_dir, state) = serving_with_authority(test_authority(epoch.clone(), 0));
    let scheduler = JobScheduler::new(state.clone(), limits(2, 4, 2, 2));

    assert_eq!(
        scheduler
            .run(Arc::new(AdvancingJob {
                epoch,
                leased: leased.clone(),
            }))
            .await
            .unwrap_err(),
        "authority_fenced: a newer authority epoch superseded this run"
    );

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

    assert!(state.admit_authority_epoch("proj", 7).await);
    assert!(state.admit_authority_epoch("proj", 0).await);
}

#[tokio::test]
async fn test_transfer_authority_home_delegates_to_the_group() {
    let (_dir, state) = serving();

    assert_eq!(state.transfer_authority_home("proj", "west").await.unwrap(), None);

    let (_dir, distributed) = serving_with_authority(test_authority(Arc::new(AtomicU64::new(5)), 0));
    assert_eq!(distributed.transfer_authority_home("proj", "west").await.unwrap(), None);
}

#[tokio::test]
async fn test_a_run_under_the_current_epoch_is_not_fenced() {
    let (_dir, state) = serving_with_authority(test_authority(Arc::new(AtomicU64::new(5)), 0));
    let scheduler = JobScheduler::new(state.clone(), limits(2, 4, 2, 2));

    scheduler
        .run(TestJob::persisting_repository(
            "steady",
            "proj",
            "proj",
            Action::Return(Ok(JobReport::default())),
        ))
        .await
        .unwrap();
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_write_ledger_reap_drains_settled_rows_and_keeps_pending() {
    let (_dir, state) = serving();
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

    let report = super::WriteLedgerReap::default()
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
    assert_eq!(super::WriteLedgerReap::default().kind(), "write_ledger_reap");
    assert_eq!(super::WriteLedgerReap::default().scope(), "");
}

#[tokio::test]
async fn test_write_ledger_reap_stops_when_cancelled() {
    let (_dir, state) = serving();
    let cancel = CancellationToken::new();
    cancel.cancel();

    let report = super::WriteLedgerReap::default()
        .run(&context(state, cancel))
        .await
        .unwrap();

    assert_eq!(report, JobReport::default());
}

#[tokio::test]
async fn test_write_ledger_reap_surfaces_a_storage_fault() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reap.redb");
    MetaStore::open(&path).unwrap();
    let meta = MetaStore::open_existing_read_only(&path).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let clock: Clock = Arc::new(|| 1_000);
    let state = AppState::with_clock(meta, blobs, 60, Vec::new(), clock).serving;

    let failure = super::WriteLedgerReap::default()
        .run(&context(state, CancellationToken::new()))
        .await
        .unwrap_err();
    assert!(failure.message().contains("read-only"), "{}", failure.message());
}

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

    fn metadata(&self) -> NodeJobMetadata<'_> {
        NodeJobMetadata {
            lease_scope: LeaseScope::ClusterSingleton(self.key.clone()),
            repository: None,
            persist_as: Some(JobKind::new("plugin_sync").unwrap()),
        }
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
    let (_dir, state) = serving_with_authority(test_authority(Arc::new(AtomicU64::new(0)), 7));
    assert_eq!(state.cluster_term(), 7);
    let scheduler = JobScheduler::new(state.clone(), limits(2, 4, 2, 2));
    let leased = Arc::new(AtomicU64::new(0));
    let ran = Arc::new(AtomicUsize::new(0));

    scheduler
        .run(SingletonJob::new("reclaim", leased.clone(), ran.clone()))
        .await
        .unwrap();

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
    let (_dir, state) = serving_with_authority(test_authority(Arc::new(AtomicU64::new(0)), 5));
    state
        .meta
        .claim_job_lease("reclaim", "node-other", 9, 1_000, 300)
        .unwrap();
    let scheduler = JobScheduler::new(state.clone(), limits(2, 4, 2, 2));
    let leased = Arc::new(AtomicU64::new(0));
    let ran = Arc::new(AtomicUsize::new(0));

    assert_eq!(
        scheduler
            .run(SingletonJob::new("reclaim", leased.clone(), ran.clone()))
            .await
            .unwrap_err(),
        "lease_not_held: a newer fence 9 supersedes the applied fence 5"
    );

    assert_eq!(ran.load(Ordering::SeqCst), 0, "a fenced run never executes its work");
    let runs = job_runs(&state.meta);
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].error.as_deref(),
        Some("lease_not_held: a newer fence 9 supersedes the applied fence 5"),
    );
    let lease = state.meta.job_lease("reclaim").unwrap().unwrap();
    assert_eq!(lease.holder, "node-other");
    assert_eq!(lease.epoch, 9);
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_a_cluster_singleton_superseded_mid_run_is_fenced() {
    let (_dir, state) = serving_with_authority(test_authority(Arc::new(AtomicU64::new(0)), 7));
    let scheduler = JobScheduler::new(state.clone(), limits(2, 4, 2, 2));
    let leased = Arc::new(AtomicU64::new(0));
    let ran = Arc::new(AtomicUsize::new(0));

    assert_eq!(
        scheduler
            .run(SingletonJob::superseding(
                "reclaim",
                leased.clone(),
                ran.clone(),
                "node-other",
                12,
            ))
            .await
            .unwrap_err(),
        "authority_fenced: a newer holder superseded this cluster-singleton run"
    );

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
    let (_dir, state) = serving_with_authority(test_authority(Arc::new(AtomicU64::new(0)), 4));
    let scheduler = JobScheduler::new(state.clone(), limits(2, 4, 2, 2));

    scheduler
        .run(TestJob::persisting(
            "cleanup",
            "",
            Action::Return(Ok(JobReport::default())),
        ))
        .await
        .unwrap();

    assert!(
        state.meta.job_leases().unwrap().is_empty(),
        "a node-local job records no lease"
    );
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_a_restarted_node_leases_the_live_term_not_a_persisted_token() {
    let (_dir, state) = serving_with_authority(test_authority(Arc::new(AtomicU64::new(0)), 6));
    state
        .meta
        .claim_job_lease("reclaim", "node-old", 5, 1_000, 300)
        .unwrap();
    state.meta.release_job_lease("reclaim", "node-old", 5).unwrap();
    let scheduler = JobScheduler::new(state.clone(), limits(2, 4, 2, 2));
    let leased = Arc::new(AtomicU64::new(0));
    let ran = Arc::new(AtomicUsize::new(0));

    scheduler
        .run(SingletonJob::new("reclaim", leased.clone(), ran.clone()))
        .await
        .unwrap();

    assert_eq!(
        leased.load(Ordering::SeqCst),
        6,
        "the run minted the live term, not the persisted token 5"
    );
    let persisted = state.meta.job_lease("reclaim").unwrap().unwrap();
    assert_eq!(persisted.epoch, 6);
    assert_eq!(persisted.state, LeaseState::Released);
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_a_cluster_singleton_without_a_group_leases_the_zero_sentinel() {
    let (_dir, state) = serving();
    let scheduler = JobScheduler::new(state.clone(), limits(2, 4, 2, 2));
    let leased = Arc::new(AtomicU64::new(9));
    let ran = Arc::new(AtomicUsize::new(0));

    scheduler
        .run(SingletonJob::new("reclaim", leased.clone(), ran.clone()))
        .await
        .unwrap();

    assert_eq!(ran.load(Ordering::SeqCst), 1);
    assert_eq!(leased.load(Ordering::SeqCst), 0, "no group leases the 0 sentinel");
    let lease = state.meta.job_lease("reclaim").unwrap().unwrap();
    assert_eq!(lease.epoch, 0);
    assert_eq!(lease.state, LeaseState::Released);
    scheduler.shutdown().await;
}
