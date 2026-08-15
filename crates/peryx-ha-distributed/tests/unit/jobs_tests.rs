use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

use async_trait::async_trait;
use peryx_driver::jobs::{JobLimits, JobReport, JobScheduler, LeaseScope, NodeJob, scheduled_job};
use peryx_driver::state::AppState;
use peryx_ha::{
    AuthorityDrainer, AvailabilityTaskError, AvailabilityTaskReport, BlobReclaimer, CrossDcCopier, PlacementReconciler,
};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::{JobKind, MetaStore};
use rstest::rstest;

use super::{
    AuthorityDrainJob, DEFAULT_DC_COPY_CONCURRENCY, DEFAULT_PLACEMENT_RECONCILE_BATCH, DEFAULT_RECLAMATION_BATCH,
    DcCopyJob, DcCopyParameters, PlacementReconcileJob, PlacementReconcileParameters, ReclamationJob,
    ReclamationParameters, compile_scheduled_job, is_scheduled_job_kind,
};

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
    let (_directory, app) = app();
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
}

#[rstest]
#[case::dc_copy(JobCase::DcCopy)]
#[case::placement(JobCase::Placement)]
#[case::reclamation(JobCase::Reclamation)]
#[tokio::test]
async fn test_distributed_job_runs_its_bound_capability(#[case] case: JobCase) {
    let (_directory, app) = app();
    let scheduler = JobScheduler::new(app.serving, JobLimits::node_local());
    let task = Arc::new(Task::new(Ok(AvailabilityTaskReport {
        processed: 5,
        changed: 2,
    })));
    let job = case.job(task.clone());
    let (kind, lease) = case.expected();

    assert_eq!((job.kind(), job.scope(), job.lease_scope()), (kind, "", lease));
    assert_eq!(
        scheduler.run(job).await.unwrap(),
        JobReport {
            processed: 5,
            changed: 2
        }
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
    now: AtomicI64,
    cancelled: AtomicBool,
}

#[async_trait]
impl AuthorityDrainer for Drainer {
    async fn drain(
        &self,
        now: i64,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<AvailabilityTaskReport, AvailabilityTaskError> {
        self.now.store(now, Ordering::SeqCst);
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

#[rstest]
#[case::success(false, Ok(JobReport { processed: 3, changed: 2 }))]
#[case::failure(true, Err("storage: unavailable".to_owned()))]
#[tokio::test]
async fn test_authority_drain_runs_through_the_scheduler(
    #[case] failed: bool,
    #[case] expected: Result<JobReport, String>,
) {
    let (_directory, app) = app();
    let scheduler = JobScheduler::new(app.serving, JobLimits::node_local());
    let drainer = Arc::new(Drainer {
        failed,
        now: AtomicI64::new(0),
        cancelled: AtomicBool::new(false),
    });
    let job = Arc::new(AuthorityDrainJob::new("resource", drainer.clone()));

    assert_eq!(job.kind(), "authority_drain");
    assert_eq!(job.scope(), "resource");
    assert_eq!(job.repository(), Some("resource"));
    assert_eq!(job.persist_as(), Some(JobKind::new("authority_drain").unwrap()));
    assert_eq!(scheduler.run(job).await, expected);
    assert_eq!(drainer.now.load(Ordering::SeqCst), 41);
    assert!(!drainer.cancelled.load(Ordering::SeqCst));
    scheduler.shutdown().await;
}
