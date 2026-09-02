use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use peryx_driver::jobs::{
    CancelJobRun, JobLimits, JobReport, JobRunOutcome, JobScheduler, LeaseScope, NodeJob, scheduled_job,
};
use peryx_driver::serving::IntentFinalizer;
use peryx_driver::state::{AppState, ServingState};
use peryx_ha::{
    AuthorityDrainer, AvailabilityCapabilities, AvailabilityTaskError, AvailabilityTaskReport, BlobReclaimer,
    CrossDcCopier, PlacementReconciler, RetainedWriteFinalizer,
};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::{JobKind, MetaStore};
use rstest::rstest;
use tokio::sync::Notify;

use super::{
    AuthorityDrainJob, DEFAULT_DC_COPY_CONCURRENCY, DEFAULT_PLACEMENT_RECONCILE_BATCH, DEFAULT_RECLAMATION_BATCH,
    DcCopyJob, DcCopyParameters, PlacementReconcileJob, PlacementReconcileParameters, ReclamationJob,
    ReclamationParameters, compile_scheduled_job, is_scheduled_job_kind,
};
use crate::support::install_distributed_services_with_capabilities;

fn app() -> (tempfile::TempDir, AppState) {
    let directory = tempfile::tempdir().unwrap();
    let state = AppState::with_clock(
        MetaStore::open(directory.path().join("peryx.redb")).unwrap(),
        BlobStore::new(directory.path().join("blobs")),
        60,
        Vec::new(),
        Arc::new(|| 41),
    );
    (directory, state)
}

fn app_with_capabilities(capabilities: AvailabilityCapabilities) -> (tempfile::TempDir, AppState) {
    let (directory, mut state) = app();
    install_distributed_services_with_capabilities(&mut state, Vec::new(), capabilities);
    (directory, state)
}

fn settings(source: &str) -> toml::Table {
    toml::from_str(source).unwrap()
}

#[rstest]
#[case::dc_copy("dc_copy", true)]
#[case::placement("placement_reconcile", true)]
#[case::reclamation("reclamation", true)]
#[case::authority("authority_drain", true)]
#[case::plugin("plugin_sync", false)]
fn test_scheduled_job_kind_recognition_is_owner_defined(#[case] kind: &str, #[case] expected: bool) {
    assert_eq!(is_scheduled_job_kind(kind), expected);
}

#[rstest]
#[case::dc_copy("dc_copy", "", "dc_copy", [("concurrency", 8)].as_slice())]
#[case::dc_copy_custom("dc_copy", "concurrency = 2", "dc_copy", &[("concurrency", 2)])]
#[case::placement("placement_reconcile", "", "placement_reconcile", &[])]
#[case::reclamation("reclamation", "", "reclamation", &[])]
fn test_compile_scheduled_job_preserves_public_identity(
    #[case] kind: &str,
    #[case] source: &str,
    #[case] expected_kind: &str,
    #[case] expected_settings: &[(&str, i64)],
) {
    let job = compile_scheduled_job(kind, &settings(source)).unwrap().unwrap();

    assert_eq!(job.as_str(), expected_kind);
    assert_eq!(
        job.settings(),
        expected_settings
            .iter()
            .map(|(key, value)| ((*key).to_owned(), toml::Value::Integer(*value)))
            .collect::<toml::Table>()
    );
}

#[rstest]
#[case::unknown("vacuum", "", None)]
#[case::authority("authority_drain", "", Some("authority drain runs only on demand"))]
#[case::dc_unknown("dc_copy", "batch = 4", Some("cross-datacenter copy accepts only `concurrency`"))]
#[case::dc_zero(
    "dc_copy",
    "concurrency = 0",
    Some("cross-datacenter copy `concurrency` must be positive")
)]
#[case::dc_type(
    "dc_copy",
    "concurrency = \"four\"",
    Some("cross-datacenter copy `concurrency` must be positive")
)]
#[case::dc_large(
    "dc_copy",
    "concurrency = 65",
    Some("cross-datacenter copy `concurrency` exceeds the per-pass limit")
)]
#[case::placement_fields(
    "placement_reconcile",
    "batch = 4",
    Some("placement reconcile accepts no job-specific fields")
)]
#[case::reclamation_fields("reclamation", "batch = 4", Some("reclamation accepts no job-specific fields"))]
fn test_compile_scheduled_job_rejects_invalid_configuration(
    #[case] kind: &str,
    #[case] source: &str,
    #[case] expected: Option<&str>,
) {
    assert_eq!(
        compile_scheduled_job(kind, &settings(source)).map(|result| result.unwrap_err()),
        expected
    );
}

#[rstest]
#[case::dc_copy("dc_copy", "cross-datacenter copy is unavailable")]
#[case::placement("placement_reconcile", "placement reconciliation is unavailable")]
#[case::reclamation("reclamation", "reclamation is unavailable")]
fn test_scheduled_job_rejects_missing_distributed_capability(#[case] kind: &str, #[case] expected: &str) {
    let (_directory, app) = app_with_capabilities(AvailabilityCapabilities::default());
    let job = compile_scheduled_job(kind, &toml::Table::new()).unwrap().unwrap();

    assert_eq!(scheduled_job(&app, &job).err().unwrap(), expected);
}

#[test]
fn test_parameter_defaults_match_runtime_bounds() {
    assert_eq!(DcCopyParameters::new().concurrency().get(), DEFAULT_DC_COPY_CONCURRENCY);
    assert_eq!(
        PlacementReconcileParameters::new().batch.get(),
        DEFAULT_PLACEMENT_RECONCILE_BATCH
    );
    assert_eq!(ReclamationParameters::new().batch.get(), DEFAULT_RECLAMATION_BATCH);
    assert_eq!(DcCopyParameters::default(), DcCopyParameters::new());
    assert_eq!(
        PlacementReconcileParameters::default(),
        PlacementReconcileParameters::new()
    );
    assert_eq!(ReclamationParameters::default(), ReclamationParameters::new());
}

struct Task {
    report: Result<AvailabilityTaskReport, AvailabilityTaskError>,
    concurrency: AtomicU64,
    batch: AtomicU64,
    fence: AtomicU64,
}

impl Task {
    fn new(report: Result<AvailabilityTaskReport, AvailabilityTaskError>) -> Self {
        Self {
            report,
            concurrency: AtomicU64::new(0),
            batch: AtomicU64::new(0),
            fence: AtomicU64::new(0),
        }
    }
}

#[async_trait]
impl CrossDcCopier for Task {
    async fn copy_pass(
        &self,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
        concurrency: NonZeroUsize,
    ) -> Result<AvailabilityTaskReport, AvailabilityTaskError> {
        assert!(!cancelled());
        self.concurrency.store(concurrency.get() as u64, Ordering::SeqCst);
        self.report.clone()
    }
}

#[async_trait]
impl PlacementReconciler for Task {
    async fn reconcile_pass(
        &self,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
        batch: NonZeroUsize,
    ) -> Result<AvailabilityTaskReport, AvailabilityTaskError> {
        assert!(!cancelled());
        self.batch.store(batch.get() as u64, Ordering::SeqCst);
        self.report.clone()
    }
}

#[async_trait]
impl BlobReclaimer for Task {
    async fn reclaim_pass(
        &self,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
        fence: u64,
        batch: NonZeroUsize,
    ) -> Result<AvailabilityTaskReport, AvailabilityTaskError> {
        assert!(!cancelled());
        self.fence.store(fence, Ordering::SeqCst);
        self.batch.store(batch.get() as u64, Ordering::SeqCst);
        self.report.clone()
    }
}

#[derive(Clone, Copy)]
enum JobCase {
    DcCopy,
    Placement,
    Reclamation,
}

impl JobCase {
    fn job(self, task: Arc<Task>) -> Arc<dyn NodeJob> {
        match self {
            Self::DcCopy => Arc::new(DcCopyJob::new(task, DcCopyParameters::new())),
            Self::Placement => Arc::new(PlacementReconcileJob::new(task, PlacementReconcileParameters::new())),
            Self::Reclamation => Arc::new(ReclamationJob::new(task, ReclamationParameters::new())),
        }
    }

    fn expected(self) -> (&'static str, LeaseScope) {
        match self {
            Self::DcCopy => ("dc_copy", LeaseScope::NodeLocal),
            Self::Placement => ("placement_reconcile", LeaseScope::NodeLocal),
            Self::Reclamation => ("reclamation", LeaseScope::ClusterSingleton("reclamation".to_owned())),
        }
    }

    fn capabilities(self, task: Arc<Task>) -> AvailabilityCapabilities {
        match self {
            Self::DcCopy => AvailabilityCapabilities {
                copier: Some(task),
                ..Default::default()
            },
            Self::Placement => AvailabilityCapabilities {
                placement: Some(task),
                ..Default::default()
            },
            Self::Reclamation => AvailabilityCapabilities {
                reclaimer: Some(task),
                ..Default::default()
            },
        }
    }
}

#[rstest]
#[case::dc_copy(JobCase::DcCopy)]
#[case::placement(JobCase::Placement)]
#[case::reclamation(JobCase::Reclamation)]
#[tokio::test]
async fn test_distributed_job_runs_its_bound_capability(#[case] case: JobCase) {
    let task = Arc::new(Task::new(Ok(AvailabilityTaskReport {
        processed: 5,
        changed: 2,
    })));
    let (kind, lease) = case.expected();
    let (_directory, app) = app_with_capabilities(case.capabilities(task.clone()));
    let registered = compile_scheduled_job(kind, &toml::Table::new()).unwrap().unwrap();
    let job = scheduled_job(&app, &registered).unwrap();
    let scheduler = JobScheduler::new(app.serving, JobLimits::node_local());

    assert_eq!((job.kind(), job.scope(), job.lease_scope()), (kind, "", lease));
    assert_eq!(
        scheduler.run(job).await.unwrap(),
        JobRunOutcome::succeeded(JobReport {
            processed: 5,
            changed: 2,
            ..JobReport::default()
        })
    );
    match case {
        JobCase::DcCopy => assert_eq!(
            task.concurrency.load(Ordering::SeqCst),
            DEFAULT_DC_COPY_CONCURRENCY as u64
        ),
        JobCase::Placement => assert_eq!(
            task.batch.load(Ordering::SeqCst),
            DEFAULT_PLACEMENT_RECONCILE_BATCH as u64
        ),
        JobCase::Reclamation => assert_eq!(
            (task.batch.load(Ordering::SeqCst), task.fence.load(Ordering::SeqCst)),
            (DEFAULT_RECLAMATION_BATCH as u64, 0)
        ),
    }
    scheduler.shutdown().await;
}

#[rstest]
#[case::dc_copy(JobCase::DcCopy)]
#[case::placement(JobCase::Placement)]
#[case::reclamation(JobCase::Reclamation)]
#[tokio::test]
async fn test_distributed_job_preserves_capability_failure(#[case] case: JobCase) {
    let (_directory, app) = app();
    let scheduler = JobScheduler::new(app.serving, JobLimits::node_local());
    let error = scheduler
        .run(case.job(Arc::new(Task::new(Err(AvailabilityTaskError::new(
            "transport",
            "peer unavailable",
        ))))))
        .await
        .unwrap_err();

    assert_eq!(error, "transport: peer unavailable");
    scheduler.shutdown().await;
}

struct Drainer {
    failed: bool,
    drained: Mutex<Vec<String>>,
    settled: AtomicBool,
    cancelled: AtomicBool,
}

impl Drainer {
    fn new(failed: bool) -> Self {
        Self {
            failed,
            drained: Mutex::new(Vec::new()),
            settled: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl AuthorityDrainer for Drainer {
    async fn drain(
        &self,
        authority: &str,
        finalizer: &dyn RetainedWriteFinalizer,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<AvailabilityTaskReport, AvailabilityTaskError> {
        self.drained.lock().unwrap().push(authority.to_owned());
        self.settled
            .store(finalizer.finalize_retained(authority, "intent").await, Ordering::SeqCst);
        self.cancelled.store(cancelled(), Ordering::SeqCst);
        if self.failed {
            Err(AvailabilityTaskError::new("storage", "unavailable"))
        } else {
            Ok(AvailabilityTaskReport {
                processed: 3,
                changed: 2,
            })
        }
    }
}

/// Stands in for an installed ecosystem: it publishes the retained writes staged under the keys it
/// minted and declines the rest, as a real finalizer does for another ecosystem's key.
struct Ecosystem {
    prefix: &'static str,
    offered: Mutex<Vec<String>>,
}

impl Ecosystem {
    fn new(prefix: &'static str) -> Arc<Self> {
        Arc::new(Self {
            prefix,
            offered: Mutex::new(Vec::new()),
        })
    }

    fn offered(&self) -> Vec<String> {
        self.offered.lock().unwrap().clone()
    }
}

#[async_trait]
impl IntentFinalizer for Ecosystem {
    async fn finalize_admitted(&self, _: Arc<ServingState>) -> u64 {
        self.offered().len() as u64
    }

    async fn finalize_retained(&self, _: Arc<ServingState>, authority: &str, intent_key: &str) -> bool {
        self.offered.lock().unwrap().push(format!("{authority}/{intent_key}"));
        intent_key.starts_with(self.prefix)
    }
}

#[rstest]
#[case::success(false, Ok(JobRunOutcome::succeeded(JobReport { processed: 3, changed: 2, ..JobReport::default() })))]
#[case::failure(true, Err("storage: unavailable".to_owned()))]
#[tokio::test]
async fn test_authority_drain_runs_through_the_scheduler(
    #[case] failed: bool,
    #[case] expected: Result<JobRunOutcome, String>,
) {
    let (_directory, app) = app();
    let serving = app.serving.clone();
    let scheduler = JobScheduler::new(app.serving, JobLimits::node_local());
    let drainer = Arc::new(Drainer::new(failed));
    let owner = Ecosystem::new("intent");
    let job = Arc::new(AuthorityDrainJob::new(
        "resource",
        drainer.clone(),
        vec![owner.clone() as Arc<dyn IntentFinalizer>],
    ));

    assert_eq!(job.kind(), "authority_drain");
    assert_eq!(job.scope(), "resource");
    assert_eq!(job.repository(), Some("resource"));
    assert_eq!(job.persist_as(), Some(JobKind::new("authority_drain").unwrap()));
    assert_eq!(scheduler.run(job).await, expected);
    assert_eq!(*drainer.drained.lock().unwrap(), vec!["resource".to_owned()]);
    assert!(drainer.settled.load(Ordering::SeqCst));
    assert!(!drainer.cancelled.load(Ordering::SeqCst));
    assert_eq!(owner.offered(), vec!["resource/intent".to_owned()]);
    assert_eq!(owner.finalize_admitted(serving).await, 1);
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_authority_drain_dispatches_a_retained_write_to_the_ecosystem_that_owns_its_key() {
    let (_directory, app) = app();
    let scheduler = JobScheduler::new(app.serving, JobLimits::node_local());
    let drainer = Arc::new(Drainer::new(false));
    let other = Ecosystem::new("other:");
    let owner = Ecosystem::new("intent");
    let last = Ecosystem::new("intent");
    let job = Arc::new(AuthorityDrainJob::new(
        "resource",
        drainer.clone(),
        vec![
            other.clone() as Arc<dyn IntentFinalizer>,
            owner.clone(),
            last.clone() as Arc<dyn IntentFinalizer>,
        ],
    ));

    scheduler.run(job).await.unwrap();

    assert!(drainer.settled.load(Ordering::SeqCst));
    assert_eq!(
        (other.offered(), owner.offered(), last.offered()),
        (
            vec!["resource/intent".to_owned()],
            vec!["resource/intent".to_owned()],
            Vec::new()
        )
    );
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_authority_drain_leaves_a_write_no_installed_ecosystem_owns_pending() {
    let (_directory, app) = app();
    let scheduler = JobScheduler::new(app.serving, JobLimits::node_local());
    let drainer = Arc::new(Drainer::new(false));
    let other = Ecosystem::new("other:");
    let job = Arc::new(AuthorityDrainJob::new(
        "resource",
        drainer.clone(),
        vec![other.clone() as Arc<dyn IntentFinalizer>],
    ));

    scheduler.run(job).await.unwrap();

    assert!(!drainer.settled.load(Ordering::SeqCst));
    assert_eq!(other.offered(), vec!["resource/intent".to_owned()]);
    scheduler.shutdown().await;
}

struct CancelledDrainer {
    started: Notify,
    release: Notify,
}

#[async_trait]
impl AuthorityDrainer for CancelledDrainer {
    async fn drain(
        &self,
        _: &str,
        _: &dyn RetainedWriteFinalizer,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<AvailabilityTaskReport, AvailabilityTaskError> {
        self.started.notify_one();
        self.release.notified().await;
        assert!(cancelled());
        Ok(AvailabilityTaskReport {
            processed: 3,
            changed: 2,
        })
    }
}

#[tokio::test]
async fn test_authority_drain_reports_observed_cancellation() {
    let (_directory, app) = app();
    let meta = app.serving.meta.clone();
    let scheduler = JobScheduler::new(app.serving, JobLimits::node_local());
    let drainer = Arc::new(CancelledDrainer {
        started: Notify::new(),
        release: Notify::new(),
    });
    let job = Arc::new(AuthorityDrainJob::new("resource", drainer.clone(), Vec::new()));

    let cancel = async {
        drainer.started.notified().await;
        let id = meta.list_job_runs().unwrap()[0].id.clone();
        assert_eq!(scheduler.cancel_job_run(&id).unwrap(), CancelJobRun::Requested);
        drainer.release.notify_one();
    };
    let (outcome, ()) = tokio::join!(scheduler.run(job), cancel);

    assert_eq!(
        outcome.unwrap(),
        JobRunOutcome::cancelled(JobReport {
            processed: 3,
            changed: 2,
            ..JobReport::default()
        })
    );
    scheduler.shutdown().await;
}
