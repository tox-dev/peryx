//! Behaviour tests for the node-local job scheduler and the maintenance job it runs.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use peryx_core::Ecosystem;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::{JobKind, JobState, MetaStore};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::scheduler::{JobLimits, Submit};
use super::{
    CACHE_MAINTENANCE, JobContext, JobReport, JobScheduler, MaintenanceJob, NodeJob, Schedule, ScheduledJob,
    run_schedules, submit_maintenance,
};
use crate::serving::{EcosystemDriver, RefreshSweep};
use crate::state::{AppState, Clock, ServingState};

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

/// The behaviour a [`TestJob`] carries out once it starts running.
enum Action {
    Return(Result<JobReport, String>),
    Block(Arc<Notify>),
    UntilCancelled,
    SleepIgnoringCancel(Duration),
}

/// A job with observable start and run count, for driving the scheduler through its states.
struct TestJob {
    kind: &'static str,
    scope: String,
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
            persist: Some(JobKind::CacheRefresh),
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

    fn persist_as(&self) -> Option<JobKind> {
        self.persist
    }

    async fn run(&self, ctx: &JobContext) -> Result<JobReport, String> {
        self.ran.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        match &self.action {
            Action::Return(result) => result.clone(),
            Action::Block(release) => {
                release.notified().await;
                Ok(JobReport::default())
            }
            Action::UntilCancelled => {
                ctx.cancelled().await;
                Ok(JobReport::default())
            }
            Action::SleepIgnoringCancel(duration) => {
                tokio::time::sleep(*duration).await;
                Ok(JobReport::default())
            }
        }
    }
}

/// An ecosystem driver whose reclaim and refresh results are fixed, counting each call.
struct StubDriver {
    ecosystem: Ecosystem,
    reclaim: usize,
    refresh: Result<RefreshSweep, String>,
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
            ecosystem: Ecosystem::Pypi,
            reclaim,
            refresh,
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
}

#[async_trait]
impl EcosystemDriver for StubDriver {
    fn ecosystem(&self) -> Ecosystem {
        self.ecosystem
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

    async fn reclaim_idle(&self, _state: Arc<ServingState>) -> usize {
        self.reclaim_calls.fetch_add(1, Ordering::SeqCst);
        self.reclaim
    }

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
    assert!(state.meta.list_job_runs().unwrap().is_empty());
    scheduler.shutdown().await;
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
async fn test_shutdown_returns_after_the_grace_period_when_a_job_ignores_cancellation() {
    let (_dir, state) = serving();
    let mut limits = limits(2, 4, 2, 2);
    limits.shutdown_grace = Duration::from_millis(50);
    let scheduler = JobScheduler::new(state, limits);
    let stubborn = TestJob::new("probe", "a", Action::SleepIgnoringCancel(Duration::from_secs(30)));
    scheduler.submit(stubborn.clone());
    stubborn.started.notified().await;
    let start = Instant::now();
    scheduler.shutdown().await;
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "shutdown waited on the stubborn job past its grace"
    );
}

#[tokio::test]
async fn test_a_failing_persisted_job_records_a_failed_run() {
    let (_dir, state) = serving();
    let scheduler = JobScheduler::new(state.clone(), limits(2, 4, 2, 2));
    let job = TestJob::persisting("cache_maintenance", "pypi", Action::Return(Err("boom".to_owned())));
    scheduler.submit(job.clone());
    job.started.notified().await;
    scheduler.shutdown().await;
    let runs = state.meta.list_job_runs().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].state, JobState::Failed);
    assert_eq!(runs[0].error.as_deref(), Some("boom"));
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
    JobContext { state, cancel }
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
    assert_eq!(job.scope(), "pypi");
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
        job.run(&context(state, CancellationToken::new())).await.unwrap_err(),
        "upstream down"
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
    state.register_ecosystem(driver, Arc::new(peryx_search::EmptyIndexer));
    let scheduler = JobScheduler::new(state.serving.clone(), JobLimits::node_local());
    submit_maintenance(&state, &scheduler);
    refresh_started.notified().await;
    scheduler.shutdown().await;
    let runs = state.serving.meta.list_job_runs().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].kind, JobKind::CacheRefresh);
    assert_eq!(runs[0].scope, "pypi");
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

    async fn run(&self, _ctx: &JobContext) -> Result<JobReport, String> {
        Ok(JobReport::default())
    }
}

#[test]
fn test_a_job_defaults_to_running_without_a_persisted_record() {
    assert_eq!(BareJob { scope: String::new() }.persist_as(), None);
}

fn scheduled_app(driver: Arc<StubDriver>) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let clock: Clock = Arc::new(|| 1_000);
    let mut state = AppState::with_clock(meta, blobs, 60, Vec::new(), clock);
    state.register_ecosystem(driver, Arc::new(peryx_search::EmptyIndexer));
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
async fn test_no_schedules_returns_at_once() {
    let (_dir, app) = scheduled_app(Arc::new(StubDriver::new(0, Ok(RefreshSweep::default()))));
    let scheduler = Arc::new(JobScheduler::new(app.serving.clone(), JobLimits::node_local()));
    run_schedules(app, scheduler, Vec::new(), CancellationToken::new()).await;
}

#[tokio::test(start_paused = true)]
async fn test_a_schedule_fires_its_job_when_the_interval_elapses() {
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
