use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use peryx_core::Ecosystem;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::{FinishJobRun, JobKind, JobOutcome, JobRunQuery, JobState, MetaStore, NewJobRun};
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
    CacheRefreshJob, CancelJobRun, IdleReclaimJob, IntentFinalizeJob, JobCompletionOutcome, JobContext, JobFailure,
    JobHistoryCleanup, JobReport, JobRunOutcome, JobScheduler, LeaseScope, NodeJob, NodeJobMetadata,
    PluginScheduledJob, RegisteredScheduledJob, Schedule, ScheduledJob, ScheduledJobFactory, SearchRebuildJob,
    run_schedules, scheduled_job, submit_maintenance,
};
use crate::serving::{CacheRefresher, IdleReclaimer, IntentFinalizer, RefreshSweep};
use crate::state::{
    AppState, Clock, ServingState, SingletonAcquisition, SingletonLease, SingletonRelease, SingletonRenewal,
};
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

    async fn run(&self, ctx: &JobContext) -> Result<JobRunOutcome, JobFailure> {
        self.ran.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        let result = match &self.action {
            Action::Return(result) => result
                .clone()
                .map(JobRunOutcome::succeeded)
                .map_err(|message| JobFailure::new("test", message)),
            Action::Block(release) => {
                release.notified().await;
                Ok(JobRunOutcome::succeeded(JobReport::default()))
            }
            Action::UntilCancelled => {
                ctx.cancelled().await;
                Ok(JobRunOutcome::cancelled(JobReport::default()))
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
                Ok(JobRunOutcome::succeeded(JobReport::default()))
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

#[derive(Clone, Default)]
struct SchedulerEvents(Arc<AtomicUsize>);

impl<Subscriber> tracing_subscriber::Layer<Subscriber> for SchedulerEvents
where
    Subscriber: tracing::Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _context: tracing_subscriber::layer::Context<'_, Subscriber>) {
        if event.metadata().target() == "peryx_driver::jobs::scheduler" {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
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
            ..JobReport::default()
        })),
    );
    assert_eq!(
        scheduler.run(job.clone()).await.unwrap(),
        JobRunOutcome::succeeded(JobReport {
            processed: 4,
            changed: 2,
            ..JobReport::default()
        })
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
        ..JobReport::default()
    };

    assert_eq!(
        scheduler
            .run(TestJob::new("probe", "a", Action::Return(Ok(report))))
            .with_subscriber(test_subscriber())
            .await
            .unwrap(),
        JobRunOutcome::succeeded(report)
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

#[tokio::test(flavor = "current_thread")]
async fn test_cancelled_jobs_emit_no_terminal_event() {
    let (_dir, state) = serving();
    let scheduler = Arc::new(JobScheduler::new(state, limits(1, 2, 1, 1)));
    let job = TestJob::new("probe", "a", Action::UntilCancelled);
    let events = SchedulerEvents::default();
    let subscriber = tracing_subscriber::registry()
        .with(LevelFilter::TRACE)
        .with(events.clone());
    let _guard = tracing::subscriber::set_default(subscriber);
    tracing::trace!(target: "peryx_driver::jobs::scheduler", "observer probe");
    assert_eq!(events.0.swap(0, Ordering::SeqCst), 1);
    let run = tokio::spawn({
        let scheduler = scheduler.clone();
        let job = job.clone();
        async move { scheduler.run(job).await }
    });
    job.started.notified().await;

    scheduler.shutdown().await;
    run.await.unwrap().unwrap();
    assert_eq!(events.0.load(Ordering::SeqCst), 0);
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
    assert_eq!(
        selected_run.await.unwrap().unwrap(),
        JobRunOutcome::cancelled(JobReport::default())
    );
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
    assert_eq!(
        other_run.await.unwrap().unwrap(),
        JobRunOutcome::cancelled(JobReport::default())
    );
}

#[tokio::test]
async fn test_cancel_job_run_cannot_relabel_completed_work() {
    let (_dir, state) = serving();
    let scheduler = Arc::new(JobScheduler::new(state.clone(), limits(1, 2, 1, 1)));
    let release = Arc::new(Notify::new());
    let job = TestJob::persisting("probe", "completed", Action::Block(release.clone()));
    let run = tokio::spawn({
        let scheduler = scheduler.clone();
        let job = job.clone();
        async move { scheduler.run(job).await }
    });
    job.started.notified().await;
    let id = job_runs(&state.meta)[0].id.clone();

    assert_eq!(scheduler.cancel_job_run(&id).unwrap(), CancelJobRun::Requested);
    release.notify_one();
    assert_eq!(
        run.await.unwrap().unwrap(),
        JobRunOutcome::succeeded(JobReport::default())
    );
    assert_eq!(state.meta.get_job_run(&id).unwrap().unwrap().state, JobState::Succeeded);
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_cancel_job_run_preserves_an_error_after_cancellation() {
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
            state: JobState::Failed,
            started_at_unix: 1_000,
            finished_at_unix: Some(1_000),
            items_processed: 0,
            items_changed: 0,
            quota_released: 0,
            quota_remaining: 0,
            error: Some("test: cancelled at boundary".to_owned()),
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
            quota_released: 0,
            quota_remaining: 0,
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

#[tokio::test]
async fn test_metrics_accumulate_failed_jobs() {
    let (_dir, state) = serving();
    let scheduler = JobScheduler::new(state, limits(2, 4, 2, 2));
    for scope in ["a", "b"] {
        assert_eq!(
            scheduler
                .run(TestJob::new("probe", scope, Action::Return(Err("boom".to_owned()))))
                .await
                .unwrap_err(),
            "test: boom"
        );
    }
    scheduler.shutdown().await;

    assert!(rendered_metrics(&scheduler).contains("peryx_jobs_finished_total{kind=\"probe\",outcome=\"failed\"} 2"));
}

#[tokio::test]
async fn test_metrics_accumulate_cancelled_jobs() {
    let (_dir, state) = serving();
    let scheduler = JobScheduler::new(state, limits(1, 4, 2, 2));
    let running = TestJob::new("probe", "a", Action::UntilCancelled);
    assert_eq!(scheduler.submit(running.clone()), Submit::Queued);
    assert_eq!(
        scheduler.submit(TestJob::new("probe", "b", Action::Return(Ok(JobReport::default())),)),
        Submit::Queued
    );
    running.started.notified().await;
    scheduler.shutdown().await;

    assert!(rendered_metrics(&scheduler).contains("peryx_jobs_finished_total{kind=\"probe\",outcome=\"cancelled\"} 2"));
}

#[tokio::test]
async fn test_metrics_accumulate_admission_rejections() {
    let (_dir, state) = serving();
    let scheduler = JobScheduler::new(state, limits(1, 1, 1, 1));
    let release = Arc::new(Notify::new());
    let running = TestJob::new("probe", "a", Action::Block(release.clone()));
    assert_eq!(scheduler.submit(running.clone()), Submit::Queued);
    running.started.notified().await;
    for scope in ["a", "a"] {
        assert_eq!(
            scheduler.submit(TestJob::new("probe", scope, Action::Return(Ok(JobReport::default())),)),
            Submit::Conflict
        );
    }
    for scope in ["b", "c"] {
        assert_eq!(
            scheduler.submit(TestJob::new("probe", scope, Action::Return(Ok(JobReport::default())),)),
            Submit::QueueFull
        );
    }
    release.notify_one();
    scheduler.shutdown().await;

    let body = rendered_metrics(&scheduler);
    assert!(
        body.contains("peryx_jobs_rejected_total{kind=\"probe\",reason=\"conflict\"} 2")
            && body.contains("peryx_jobs_rejected_total{kind=\"probe\",reason=\"queue_full\"} 2")
    );
}

fn rendered_metrics(scheduler: &JobScheduler) -> String {
    let mut body = String::new();
    crate::state::PrometheusSource::write_metrics(scheduler.metrics().as_ref(), &mut body);
    body
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
        JobRunOutcome::succeeded(JobReport {
            processed: 2,
            changed: 2,
            ..JobReport::default()
        })
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
        JobRunOutcome::succeeded(JobReport {
            processed: 3,
            changed: 3,
            ..JobReport::default()
        })
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
        JobRunOutcome::succeeded(JobReport {
            processed: 3,
            changed: 1,
            ..JobReport::default()
        })
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
            JobRunOutcome::cancelled(JobReport::default())
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

    async fn run(&self, _ctx: &JobContext) -> Result<JobRunOutcome, JobFailure> {
        Ok(JobRunOutcome::succeeded(JobReport::default()))
    }
}

#[tokio::test]
async fn test_a_node_local_job_runs_without_a_persisted_record() {
    let (_dir, state) = serving();
    let scheduler = JobScheduler::new(state.clone(), JobLimits::node_local());
    let job = BareJob { scope: String::new() };

    assert_eq!((job.repository(), job.persist_as()), (None, None));
    assert_eq!(
        scheduler.run(Arc::new(job)).await.unwrap(),
        JobRunOutcome::succeeded(JobReport::default())
    );
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
        JobRunOutcome::succeeded(JobReport {
            processed: 8,
            changed: 8,
            ..JobReport::default()
        })
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

    assert_eq!(report, JobRunOutcome::cancelled(JobReport::default()));
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

fn schedule_settings(value: &str) -> toml::Table {
    toml::Table::from_iter([("mode".to_owned(), toml::Value::String(value.to_owned()))])
}

struct TestScheduleFactory {
    kind: &'static str,
    settings: toml::Table,
    job: Result<Arc<dyn NodeJob>, String>,
}

impl TestScheduleFactory {
    fn new(job: Result<Arc<dyn NodeJob>, String>) -> Self {
        Self {
            kind: "plugin_sync",
            settings: toml::Table::new(),
            job,
        }
    }

    fn identity(kind: &'static str, setting: &str) -> Self {
        Self {
            kind,
            settings: schedule_settings(setting),
            job: Ok(TestJob::new(kind, "identity", Action::Return(Ok(JobReport::default())))),
        }
    }
}

impl ScheduledJobFactory for TestScheduleFactory {
    fn kind(&self) -> &'static str {
        self.kind
    }

    fn settings(&self) -> toml::Table {
        self.settings.clone()
    }

    fn create(&self, _app: &AppState) -> Result<Arc<dyn NodeJob>, String> {
        self.job.clone()
    }
}

fn plugin_schedule(secs: u64, job: Result<Arc<dyn NodeJob>, String>) -> Vec<Schedule> {
    vec![Schedule {
        job: ScheduledJob::Plugin(PluginScheduledJob::new(
            Ecosystem::new("example"),
            Arc::new(TestScheduleFactory::new(job)),
        )),
        interval: Duration::from_secs(secs),
    }]
}

#[test]
fn test_plugin_schedule_equality_uses_its_public_identity() {
    let left = PluginScheduledJob::new(
        Ecosystem::new("example"),
        Arc::new(TestScheduleFactory::new(Ok(TestJob::new(
            "plugin_sync",
            "alpha",
            Action::Return(Ok(JobReport::default())),
        )))),
    );
    let right = PluginScheduledJob::new(
        Ecosystem::new("example"),
        Arc::new(TestScheduleFactory::new(Ok(TestJob::new(
            "plugin_sync",
            "beta",
            Action::Return(Ok(JobReport::default())),
        )))),
    );

    assert_eq!(left, right);
}

#[rstest]
#[case::ecosystem("other", "plugin_sync", "stable")]
#[case::kind("example", "other", "stable")]
#[case::settings("example", "plugin_sync", "fast")]
fn test_plugin_schedule_equality_rejects_each_identity_difference(
    #[case] ecosystem: &'static str,
    #[case] kind: &'static str,
    #[case] setting: &str,
) {
    let left = PluginScheduledJob::new(
        Ecosystem::new("example"),
        Arc::new(TestScheduleFactory::identity("plugin_sync", "stable")),
    );
    let right = PluginScheduledJob::new(
        Ecosystem::new(ecosystem),
        Arc::new(TestScheduleFactory::identity(kind, setting)),
    );

    assert_ne!(left, right);
}

#[test]
fn test_plugin_schedule_debug_names_its_public_identity() {
    let schedule = PluginScheduledJob::new(
        Ecosystem::new("example"),
        Arc::new(TestScheduleFactory::new(Ok(TestJob::new(
            "plugin_sync",
            "alpha",
            Action::Return(Ok(JobReport::default())),
        )))),
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
    let factory = Arc::new(TestScheduleFactory::new(Ok(TestJob::new(
        "plugin_sync",
        "alpha",
        Action::Return(Ok(JobReport::default())),
    ))));
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

#[rstest]
#[case::kind("other", "stable")]
#[case::settings("plugin_sync", "fast")]
fn test_registered_schedule_equality_rejects_each_identity_difference(
    #[case] kind: &'static str,
    #[case] setting: &str,
) {
    let left = RegisteredScheduledJob::new(Arc::new(TestScheduleFactory::identity("plugin_sync", "stable")));
    let right = RegisteredScheduledJob::new(Arc::new(TestScheduleFactory::identity(kind, setting)));

    assert_ne!(left, right);
}

#[test]
fn test_scheduled_job_settings_follow_the_selected_factory() {
    let factory = Arc::new(TestScheduleFactory::identity("plugin_sync", "fast"));
    let jobs = [
        ScheduledJob::Plugin(PluginScheduledJob::new(Ecosystem::new("example"), factory.clone())),
        ScheduledJob::Registered(RegisteredScheduledJob::new(factory)),
    ];

    assert!(jobs.into_iter().all(|job| job.settings() == schedule_settings("fast")));
}

#[test]
fn test_cache_maintenance_has_no_settings() {
    assert!(ScheduledJob::CacheMaintenance.settings().is_empty());
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
async fn test_an_unsupported_scheduled_job_is_rejected_without_plugin_submission() {
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
    assert_eq!(
        job_runs(&app.serving.meta)
            .iter()
            .map(|run| run.kind.as_str())
            .collect::<Vec<_>>(),
        ["write_ledger_reap", "write_ledger_reap"]
    );
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

#[rstest]
#[case::on_time(0, 60)]
#[case::within_interval(30, 60)]
#[case::past_interval(200, 260)]
fn test_reschedule_maintains_cadence_or_collapses_missed_runs(#[case] wake_secs: u64, #[case] expected_secs: u64) {
    let base = tokio::time::Instant::now();
    assert_eq!(
        super::timer::reschedule(base, base + Duration::from_secs(wake_secs), Duration::from_mins(1)),
        base + Duration::from_secs(expected_secs)
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
    let mut completions = scheduler.subscribe_completions();
    let cancel = CancellationToken::new();
    let timer = start_timer(
        app.clone(),
        scheduler.clone(),
        super::timer::ScheduleTimer::new(Vec::new(), 16),
        cancel.clone(),
    )
    .await;

    let completed = [completions.recv().await.unwrap(), completions.recv().await.unwrap()];
    let cleanup = completed
        .into_iter()
        .find(|event| event.kind() == "job_history_cleanup")
        .unwrap();
    assert_eq!(cleanup.outcome(), JobCompletionOutcome::Succeeded);
    let removed = cleanup.report().unwrap().changed;
    assert!(removed > 0);

    cancel.cancel();
    timer.await.unwrap();
    scheduler.shutdown().await;
    let remaining = u64::try_from(job_runs(&app.serving.meta).len()).unwrap();
    assert!((16..=17).contains(&remaining));
    assert_eq!(remaining + removed, 18);
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
    start_timer(
        app,
        scheduler,
        super::timer::ScheduleTimer::new(plan, super::MAX_JOB_RUNS),
        cancel,
    )
    .await
}

async fn start_timer(
    app: Arc<AppState>,
    scheduler: Arc<JobScheduler>,
    timer: super::timer::ScheduleTimer,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let (started, ready) = oneshot::channel();
    let timer = tokio::spawn(async move {
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
        JobRunOutcome::succeeded(JobReport {
            processed: 2,
            changed: 2,
            ..JobReport::default()
        })
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

    assert_eq!(report, JobRunOutcome::cancelled(JobReport::default()));
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
/// Stands in for the ownership group: the committed epoch every node reads, and one committed grant
/// table that every node pointing at it contends over, so two serving states with their own data
/// directories still compete for a single job.
#[derive(Default)]
struct TestAuthority {
    epoch: Arc<AtomicU64>,
    term: u64,
    claims: Arc<AtomicUsize>,
    grants: Mutex<HashMap<String, TestGrant>>,
    /// Notified after each renewal so a test can wait for one instead of sleeping.
    renewed: Notify,
    renewals: AtomicUsize,
    failure: GroupFailure,
}

/// Which round trip to the group is partitioned away.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum GroupFailure {
    #[default]
    None,
    Claim,
    Renew,
    Release,
}

#[derive(Clone, Default)]
struct TestGrant {
    holder: String,
    term: u64,
    generation: u64,
    held: bool,
}

fn test_authority(epoch: Arc<AtomicU64>, term: u64) -> Arc<TestAuthority> {
    Arc::new(TestAuthority {
        epoch,
        term,
        ..TestAuthority::default()
    })
}

impl TestAuthority {
    fn leasing(term: u64) -> Arc<Self> {
        test_authority(Arc::new(AtomicU64::new(0)), term)
    }

    fn partitioned(term: u64, failure: GroupFailure) -> Arc<Self> {
        Arc::new(Self {
            term,
            failure,
            ..Self::default()
        })
    }

    fn unreachable_for(&self, call: GroupFailure) -> Option<crate::state::OwnershipError> {
        (self.failure == call)
            .then(|| crate::state::OwnershipError::Unavailable("the ownership group is unreachable".to_owned()))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, TestGrant>> {
        self.grants.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Commit another holder's claim, as a competing node's acquisition would.
    fn take_over(&self, job: &str, holder: &str) {
        let mut grants = self.lock();
        let grant = grants.entry(job.to_owned()).or_default();
        grant.holder = holder.to_owned();
        grant.term = self.term;
        grant.generation += 1;
        grant.held = true;
        drop(grants);
    }

    fn owns(&self, lease: &SingletonLease) -> bool {
        self.lock().get(&lease.job).is_some_and(|grant| {
            grant.held
                && grant.holder == lease.holder
                && grant.term == lease.term
                && grant.generation == lease.generation
        })
    }

    fn holder_of(&self, job: &str) -> Option<String> {
        self.lock()
            .get(job)
            .filter(|grant| grant.held)
            .map(|grant| grant.holder.clone())
    }
}

#[async_trait]
impl crate::state::OwnershipAuthority for TestAuthority {
    async fn claim_home(&self, _authority: &str) -> Result<crate::state::HomeClaim, crate::state::OwnershipError> {
        self.claims.fetch_add(1, Ordering::SeqCst);
        Ok(crate::state::HomeClaim {
            home: "east".to_owned(),
            epoch: self.epoch.load(Ordering::SeqCst),
        })
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

    async fn acquire_singleton_lease(
        &self,
        job: &str,
        holder: &str,
    ) -> Result<SingletonAcquisition, crate::state::OwnershipError> {
        if let Some(error) = self.unreachable_for(GroupFailure::Claim) {
            return Err(error);
        }
        if let Some(current) = self.holder_of(job) {
            return Ok(SingletonAcquisition::Held { holder: current });
        }
        self.take_over(job, holder);
        let grant = self.lock()[job].clone();
        Ok(SingletonAcquisition::Acquired(SingletonLease {
            job: job.to_owned(),
            holder: grant.holder,
            term: grant.term,
            generation: grant.generation,
            expires_at_unix: i64::MAX,
        }))
    }

    async fn renew_singleton_lease(
        &self,
        lease: &SingletonLease,
    ) -> Result<SingletonRenewal, crate::state::OwnershipError> {
        self.renewals.fetch_add(1, Ordering::SeqCst);
        self.renewed.notify_one();
        if let Some(error) = self.unreachable_for(GroupFailure::Renew) {
            return Err(error);
        }
        Ok(if self.owns(lease) {
            SingletonRenewal::Renewed(lease.clone())
        } else {
            SingletonRenewal::Lost
        })
    }

    async fn release_singleton_lease(
        &self,
        lease: &SingletonLease,
    ) -> Result<SingletonRelease, crate::state::OwnershipError> {
        if let Some(error) = self.unreachable_for(GroupFailure::Release) {
            return Err(error);
        }
        if !self.owns(lease) {
            return Ok(SingletonRelease::Lost);
        }
        self.lock()
            .get_mut(&lease.job)
            .expect("an owned grant is recorded")
            .held = false;
        Ok(SingletonRelease::Released)
    }

    async fn transfer_home(
        &self,
        _authority: &str,
        _new_home: &str,
    ) -> Result<Option<crate::state::TransferOutcome>, crate::state::OwnershipError> {
        Ok(None)
    }
}

#[tokio::test]
async fn test_first_publish_home_resolves_the_authority() {
    let claims = Arc::new(AtomicUsize::new(0));
    let (_dir, state) = serving_with_authority(Arc::new(TestAuthority {
        claims: claims.clone(),
        ..TestAuthority::default()
    }));

    state.claim_first_publish_home("proj").await.unwrap();

    assert_eq!(claims.load(Ordering::SeqCst), 1);
}

struct AdvancingJob {
    epoch: Arc<AtomicU64>,
    leased: Arc<AtomicU64>,
    failure: Option<&'static str>,
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

    async fn run(&self, ctx: &JobContext) -> Result<JobRunOutcome, JobFailure> {
        self.leased.store(ctx.authority_fence(), Ordering::SeqCst);
        self.epoch.fetch_add(1, Ordering::SeqCst);
        self.failure.map_or_else(
            || Ok(JobRunOutcome::succeeded(JobReport::default())),
            |message| Err(JobFailure::new("test", message)),
        )
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
                failure: None,
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
async fn test_a_failed_run_preserves_its_error_when_authority_advances() {
    let epoch = Arc::new(AtomicU64::new(5));
    let (_dir, state) = serving_with_authority(test_authority(epoch.clone(), 0));
    let scheduler = JobScheduler::new(state, limits(2, 4, 2, 2));

    assert_eq!(
        scheduler
            .run(Arc::new(AdvancingJob {
                epoch,
                leased: Arc::new(AtomicU64::new(0)),
                failure: Some("failed at boundary"),
            }))
            .await
            .unwrap_err(),
        "test: failed at boundary"
    );
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_a_repository_job_at_epoch_zero_is_not_fenced() {
    let (_dir, state) = serving_with_authority(test_authority(Arc::new(AtomicU64::new(0)), 0));
    let scheduler = JobScheduler::new(state, limits(2, 4, 2, 2));

    assert_eq!(
        scheduler
            .run(TestJob::persisting_repository(
                "steady",
                "proj",
                "proj",
                Action::Return(Ok(JobReport::default())),
            ))
            .await,
        Ok(JobRunOutcome::succeeded(JobReport::default()))
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
        report.report().processed,
        2,
        "one admitted intent and one finalized operation reaped"
    );
    assert_eq!(state.meta.staged_intent("done").unwrap(), None);
    assert_eq!(
        state.meta.staged_intent("live").unwrap().unwrap().phase,
        peryx_storage::meta::IntentPhase::Pending
    );
    assert_eq!(state.meta.operation_outcome("op").unwrap(), None);
    assert_eq!(
        (
            super::WriteLedgerReap::default().kind(),
            super::WriteLedgerReap::default().scope(),
            super::WriteLedgerReap::default().persist_as(),
        ),
        (
            "write_ledger_reap",
            "",
            Some(JobKind::new("write_ledger_reap").unwrap()),
        )
    );
}

#[tokio::test]
async fn test_write_ledger_reap_repairs_old_quota_and_keeps_young_owner() {
    let (_dir, state) = serving();
    for (digest, bytes, created_at_unix) in [("sha256:old-a", 7, -3_000), ("sha256:old-b", 7, -2_999)] {
        state
            .meta
            .reserve_quota(
                peryx_storage::meta::NewQuotaReservation {
                    repository: "private",
                    resource: Some(digest),
                    group: None,
                    digest,
                    bytes,
                    class: peryx_storage::meta::AccountingClass::Hosted,
                    created_at_unix,
                },
                peryx_storage::meta::QuotaLimits::default(),
            )
            .unwrap();
    }
    let young = state
        .meta
        .reserve_quota(
            peryx_storage::meta::NewQuotaReservation {
                repository: "private",
                resource: Some("young"),
                group: None,
                digest: "sha256:young",
                bytes: 5,
                class: peryx_storage::meta::AccountingClass::Hosted,
                created_at_unix: 999,
            },
            peryx_storage::meta::QuotaLimits::default(),
        )
        .unwrap();

    let scheduler = JobScheduler::new(state.clone(), limits(1, 1, 1, 1));
    let report = scheduler
        .run(Arc::new(super::WriteLedgerReap { batch: 1 }))
        .await
        .unwrap();
    let run = job_runs(&state.meta).remove(0);
    scheduler.shutdown().await;

    assert_eq!(
        (
            report,
            (run.kind, run.state, run.quota_released, run.quota_remaining),
            state.meta.commit_quota_reservation(young.id).unwrap(),
            state.meta.quota_usage("private").unwrap().accounted_bytes,
        ),
        (
            JobRunOutcome::succeeded(JobReport {
                processed: 2,
                changed: 1,
                quota_released: 1,
                quota_remaining: 1,
            }),
            (JobKind::new("write_ledger_reap").unwrap(), JobState::Succeeded, 1, 1),
            true,
            peryx_storage::meta::QuotaValue {
                committed: 5,
                reserved: 7,
            },
        )
    );
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

    assert_eq!(report, JobRunOutcome::cancelled(JobReport::default()));
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

/// What a singleton run's body does while it holds the grant.
enum SingletonAction {
    Succeed,
    Fail,
    /// Hold the grant until the test releases the body.
    Block(Arc<Notify>),
    /// Wait for the run to be cancelled, which is how losing the grant reaches the body.
    UntilCancelled,
    /// Commit a competing holder's claim from inside the body.
    TakenOver(Arc<TestAuthority>),
}

#[derive(Default)]
struct Observed {
    /// Signalled once the body is running, so a test never has to poll for it.
    entered: Notify,
    runs: AtomicUsize,
    fence: AtomicU64,
    cancelled: AtomicBool,
}

struct SingletonJob {
    key: String,
    action: SingletonAction,
    observed: Arc<Observed>,
}

impl SingletonJob {
    fn new(key: &str, action: SingletonAction) -> (Arc<Self>, Arc<Observed>) {
        let observed = Arc::new(Observed::default());
        (
            Arc::new(Self {
                key: key.to_owned(),
                action,
                observed: observed.clone(),
            }),
            observed,
        )
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

    async fn run(&self, ctx: &JobContext) -> Result<JobRunOutcome, JobFailure> {
        self.observed.runs.fetch_add(1, Ordering::SeqCst);
        self.observed.fence.store(ctx.authority_fence(), Ordering::SeqCst);
        self.observed.entered.notify_one();
        match &self.action {
            SingletonAction::Succeed => {}
            SingletonAction::Fail => return Err(JobFailure::new("body", "the work failed")),
            SingletonAction::Block(release) => release.notified().await,
            SingletonAction::UntilCancelled => {
                ctx.cancelled().await;
                self.observed.cancelled.store(true, Ordering::SeqCst);
                return Ok(JobRunOutcome::cancelled(JobReport::default()));
            }
            SingletonAction::TakenOver(group) => group.take_over(&self.key, "node-other"),
        }
        Ok(JobRunOutcome::succeeded(JobReport::default()))
    }
}

const SINGLETON: &str = "reclaim";

#[tokio::test]
async fn test_two_nodes_sharing_one_group_cannot_both_enter_a_singleton_body() {
    let group = TestAuthority::leasing(7);
    let (_first_dir, first) = serving_with_authority(group.clone());
    let (_second_dir, second) = serving_with_authority(group.clone());
    let holding = Arc::new(JobScheduler::new(first, limits(2, 4, 2, 2)));
    let contending = JobScheduler::new(second.clone(), limits(2, 4, 2, 2));
    let release = Arc::new(Notify::new());
    let (job, held) = SingletonJob::new(SINGLETON, SingletonAction::Block(release.clone()));
    let running = tokio::spawn({
        let holding = holding.clone();
        async move { holding.run(job).await }
    });
    held.entered.notified().await;

    let (job, refused) = SingletonJob::new(SINGLETON, SingletonAction::Succeed);
    let error = contending.run(job).await.unwrap_err();

    assert_eq!(refused.runs.load(Ordering::SeqCst), 0);
    assert_eq!(
        error,
        format!(
            "lease_not_held: {} holds the {SINGLETON} cluster-singleton lease",
            group.holder_of(SINGLETON).unwrap()
        )
    );
    assert_eq!(
        job_runs(&second.meta)[0].error.as_deref(),
        Some(error.as_str()),
        "the refused claim is recorded in durable history"
    );
    release.notify_one();
    running.await.unwrap().unwrap();
    holding.shutdown().await;
    contending.shutdown().await;
}

#[tokio::test]
async fn test_a_singleton_run_fences_its_writes_with_the_granted_term() {
    let group = TestAuthority::leasing(7);
    let (_dir, state) = serving_with_authority(group);
    let scheduler = JobScheduler::new(state, limits(2, 4, 2, 2));
    let (job, observed) = SingletonJob::new(SINGLETON, SingletonAction::Succeed);

    scheduler.run(job).await.unwrap();

    assert_eq!(observed.runs.load(Ordering::SeqCst), 1);
    assert_eq!(observed.fence.load(Ordering::SeqCst), 7);
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_a_finished_run_frees_the_singleton_at_a_higher_generation() {
    let group = TestAuthority::leasing(7);
    let (_dir, state) = serving_with_authority(group.clone());
    let scheduler = JobScheduler::new(state, limits(2, 4, 2, 2));
    let (first, _) = SingletonJob::new(SINGLETON, SingletonAction::Succeed);
    scheduler.run(first).await.unwrap();

    let (second, observed) = SingletonJob::new(SINGLETON, SingletonAction::Succeed);
    scheduler.run(second).await.unwrap();

    assert_eq!(observed.runs.load(Ordering::SeqCst), 1);
    assert_eq!(group.lock()[SINGLETON].generation, 2);
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_a_failed_body_still_frees_the_singleton() {
    let group = TestAuthority::leasing(7);
    let (_dir, state) = serving_with_authority(group.clone());
    let scheduler = JobScheduler::new(state, limits(2, 4, 2, 2));
    let (job, _) = SingletonJob::new(SINGLETON, SingletonAction::Fail);

    assert_eq!(scheduler.run(job).await.unwrap_err(), "body: the work failed");

    assert_eq!(group.holder_of(SINGLETON), None, "a failed run gives the grant back");
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_a_claim_the_group_cannot_commit_records_its_failure() {
    let (_dir, state) = serving_with_authority(TestAuthority::partitioned(7, GroupFailure::Claim));
    let scheduler = JobScheduler::new(state, limits(2, 4, 2, 2));
    let (job, observed) = SingletonJob::new(SINGLETON, SingletonAction::Succeed);

    assert_eq!(
        scheduler.run(job).await.unwrap_err(),
        "lease_not_held: ownership claim did not commit: the ownership group is unreachable"
    );

    assert_eq!(observed.runs.load(Ordering::SeqCst), 0);
    scheduler.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn test_a_run_longer_than_one_lease_period_keeps_renewing_its_grant() {
    let group = TestAuthority::leasing(7);
    let (_dir, state) = serving_with_authority(group.clone());
    let scheduler = JobScheduler::new(state, limits(2, 4, 2, 2));
    let release = Arc::new(Notify::new());
    let (job, observed) = SingletonJob::new(SINGLETON, SingletonAction::Block(release.clone()));
    let running = tokio::spawn({
        let scheduler = Arc::new(scheduler);
        let handle = scheduler.clone();
        async move {
            let outcome = handle.run(job).await;
            (scheduler, outcome)
        }
    });
    observed.entered.notified().await;

    group.renewed.notified().await;

    assert!(group.renewals.load(Ordering::SeqCst) >= 1);
    assert!(
        group.holder_of(SINGLETON).is_some(),
        "the grant stays held across renewals"
    );
    release.notify_one();
    let (scheduler, outcome) = running.await.unwrap();
    outcome.unwrap();
    scheduler.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn test_losing_the_grant_cancels_the_run_and_fences_its_outcome() {
    let group = TestAuthority::leasing(7);
    let (_dir, state) = serving_with_authority(group.clone());
    let scheduler = JobScheduler::new(state, limits(2, 4, 2, 2));
    let (job, observed) = SingletonJob::new(SINGLETON, SingletonAction::UntilCancelled);
    let running = tokio::spawn({
        let scheduler = Arc::new(scheduler);
        let handle = scheduler.clone();
        async move {
            let outcome = handle.run(job).await;
            (scheduler, outcome)
        }
    });
    observed.entered.notified().await;

    group.take_over(SINGLETON, "node-other");
    let (scheduler, outcome) = running.await.unwrap();

    assert!(
        observed.cancelled.load(Ordering::SeqCst),
        "the run observed cancellation"
    );
    assert_eq!(
        outcome.unwrap_err(),
        "lease_fenced: ownership of this cluster-singleton run moved to another holder"
    );
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_a_grant_taken_over_mid_run_fences_a_succeeded_body() {
    let group = TestAuthority::leasing(7);
    let (_dir, state) = serving_with_authority(group.clone());
    let scheduler = JobScheduler::new(state.clone(), limits(2, 4, 2, 2));
    let (job, observed) = SingletonJob::new(SINGLETON, SingletonAction::TakenOver(group));

    let error = scheduler.run(job).await.unwrap_err();

    assert_eq!(observed.runs.load(Ordering::SeqCst), 1, "the body ran before it lost");
    assert_eq!(
        error,
        "lease_fenced: ownership of this cluster-singleton run moved to another holder"
    );
    assert_eq!(job_runs(&state.meta)[0].error.as_deref(), Some(error.as_str()));
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_a_release_the_group_cannot_commit_leaves_the_outcome_alone() {
    let group = TestAuthority::partitioned(7, GroupFailure::Release);
    let (_dir, state) = serving_with_authority(group);
    let scheduler = JobScheduler::new(state, limits(2, 4, 2, 2));
    let (job, _) = SingletonJob::new(SINGLETON, SingletonAction::Succeed);

    assert_eq!(
        scheduler.run(job).await.unwrap(),
        JobRunOutcome::succeeded(JobReport::default()),
        "cleanup that cannot reach consensus never rewrites a finished body"
    );
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_a_cluster_singleton_without_a_group_runs_unowned() {
    let (_dir, state) = serving();
    let scheduler = JobScheduler::new(state, limits(2, 4, 2, 2));
    let (job, observed) = SingletonJob::new(SINGLETON, SingletonAction::Succeed);

    scheduler.run(job).await.unwrap();

    assert_eq!(observed.runs.load(Ordering::SeqCst), 1);
    assert_eq!(
        observed.fence.load(Ordering::SeqCst),
        0,
        "a process with no group runs under the closed sentinel"
    );
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_a_node_local_job_takes_no_singleton_grant() {
    let group = TestAuthority::leasing(4);
    let (_dir, state) = serving_with_authority(group.clone());
    let scheduler = JobScheduler::new(state, limits(2, 4, 2, 2));

    scheduler
        .run(TestJob::persisting(
            "cleanup",
            "",
            Action::Return(Ok(JobReport::default())),
        ))
        .await
        .unwrap();

    assert!(group.lock().is_empty(), "a node-local job claims no grant");
    scheduler.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn test_a_renewal_that_cannot_reach_the_group_keeps_the_run_going() {
    let group = TestAuthority::partitioned(7, GroupFailure::Renew);
    let (_dir, state) = serving_with_authority(group.clone());
    let scheduler = Arc::new(JobScheduler::new(state, limits(2, 4, 2, 2)));
    let release = Arc::new(Notify::new());
    let (job, observed) = SingletonJob::new(SINGLETON, SingletonAction::Block(release.clone()));
    let running = tokio::spawn({
        let handle = scheduler.clone();
        async move { handle.run(job).await }
    });
    observed.entered.notified().await;

    group.renewed.notified().await;
    group.renewed.notified().await;
    release.notify_one();

    assert!(
        group.renewals.load(Ordering::SeqCst) >= 2,
        "a failed renewal is retried rather than treated as lost"
    );
    assert!(
        !observed.cancelled.load(Ordering::SeqCst),
        "a renewal the group cannot answer does not stop the run"
    );
    assert_eq!(
        running.await.unwrap().unwrap(),
        JobRunOutcome::succeeded(JobReport::default()),
        "only the committed answer fences a run, never a failed round trip"
    );
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_the_serving_state_hands_back_the_installed_ownership_group() {
    let group: Arc<dyn crate::state::OwnershipAuthority> = TestAuthority::leasing(7);
    let (_dir, state) = serving_with_authority(group.clone());

    let installed = state.ownership_authority().expect("the group is installed");

    assert!(Arc::ptr_eq(installed, &group));
    assert_eq!(installed.cluster_status().term, 7);
}
