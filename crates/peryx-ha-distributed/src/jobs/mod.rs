mod authority_drain;

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use peryx_driver::jobs::{
    JobContext, JobFailure, JobReport, JobRunOutcome, LeaseScope, NodeJob, NodeJobMetadata, RegisteredScheduledJob,
    ScheduledJob, ScheduledJobFactory,
};
use peryx_driver::state::AppState;
use peryx_ha::{AvailabilityTaskReport, BlobReclaimer, CrossDcCopier, PlacementReconciler};

pub use authority_drain::AuthorityDrainJob;

pub const DEFAULT_DC_COPY_CONCURRENCY: usize = 8;
pub const MAX_DC_COPY_CONCURRENCY: usize = 64;
pub const DEFAULT_PLACEMENT_RECONCILE_BATCH: usize = 256;
pub const DEFAULT_RECLAMATION_BATCH: usize = 256;

const AUTHORITY_DRAIN: &str = "authority_drain";
const DC_COPY: &str = "dc_copy";
const PLACEMENT_RECONCILE: &str = "placement_reconcile";
const RECLAMATION: &str = "reclamation";

struct CancellationProbe<'a> {
    context: &'a JobContext,
    observed: AtomicBool,
}

impl<'a> CancellationProbe<'a> {
    const fn new(context: &'a JobContext) -> Self {
        Self {
            context,
            observed: AtomicBool::new(false),
        }
    }

    fn is_cancelled(&self) -> bool {
        let cancelled = self.context.is_cancelled();
        self.observed.fetch_or(cancelled, Ordering::Relaxed);
        cancelled
    }

    fn outcome(&self, report: AvailabilityTaskReport) -> JobRunOutcome {
        if self.observed.load(Ordering::Relaxed) {
            JobRunOutcome::cancelled(task_report(report))
        } else {
            JobRunOutcome::succeeded(task_report(report))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DcCopyParameters {
    concurrency: NonZeroUsize,
}

impl DcCopyParameters {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            concurrency: NonZeroUsize::MIN.saturating_add(DEFAULT_DC_COPY_CONCURRENCY - 1),
        }
    }

    #[must_use]
    pub const fn concurrency(self) -> NonZeroUsize {
        self.concurrency
    }
}

impl Default for DcCopyParameters {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlacementReconcileParameters {
    pub batch: NonZeroUsize,
}

impl PlacementReconcileParameters {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            batch: NonZeroUsize::MIN.saturating_add(DEFAULT_PLACEMENT_RECONCILE_BATCH - 1),
        }
    }
}

impl Default for PlacementReconcileParameters {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReclamationParameters {
    pub batch: NonZeroUsize,
}

impl ReclamationParameters {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            batch: NonZeroUsize::MIN.saturating_add(DEFAULT_RECLAMATION_BATCH - 1),
        }
    }
}

impl Default for ReclamationParameters {
    fn default() -> Self {
        Self::new()
    }
}

#[must_use]
pub fn is_scheduled_job_kind(kind: &str) -> bool {
    matches!(kind, AUTHORITY_DRAIN | DC_COPY | PLACEMENT_RECONCILE | RECLAMATION)
}

#[must_use]
pub fn compile_scheduled_job(kind: &str, settings: &toml::Table) -> Option<Result<ScheduledJob, &'static str>> {
    let factory: Result<Arc<dyn ScheduledJobFactory>, _> = match kind {
        DC_COPY => compile_dc_copy(settings).map(|parameters| Arc::new(DcCopyFactory(parameters)) as _),
        PLACEMENT_RECONCILE => empty_settings(settings, "placement reconcile accepts no job-specific fields")
            .map(|()| Arc::new(PlacementReconcileFactory(PlacementReconcileParameters::new())) as _),
        RECLAMATION => empty_settings(settings, "reclamation accepts no job-specific fields")
            .map(|()| Arc::new(ReclamationFactory(ReclamationParameters::new())) as _),
        AUTHORITY_DRAIN => Err("authority drain runs only on demand"),
        _ => return None,
    };
    Some(factory.map(|factory| ScheduledJob::Registered(RegisteredScheduledJob::new(factory))))
}

fn empty_settings(settings: &toml::Table, error: &'static str) -> Result<(), &'static str> {
    if settings.is_empty() { Ok(()) } else { Err(error) }
}

fn compile_dc_copy(settings: &toml::Table) -> Result<DcCopyParameters, &'static str> {
    if settings.keys().any(|key| key != "concurrency") {
        return Err("cross-datacenter copy accepts only `concurrency`");
    }
    let Some(value) = settings.get("concurrency") else {
        return Ok(DcCopyParameters::new());
    };
    let Some(value) = value.as_integer() else {
        return Err("cross-datacenter copy `concurrency` must be positive");
    };
    let concurrency = usize::try_from(value)
        .ok()
        .and_then(NonZeroUsize::new)
        .ok_or("cross-datacenter copy `concurrency` must be positive")?;
    if concurrency.get() > MAX_DC_COPY_CONCURRENCY {
        return Err("cross-datacenter copy `concurrency` exceeds the per-pass limit");
    }
    Ok(DcCopyParameters { concurrency })
}

struct DcCopyFactory(DcCopyParameters);

impl ScheduledJobFactory for DcCopyFactory {
    fn kind(&self) -> &'static str {
        DC_COPY
    }

    fn settings(&self) -> toml::Table {
        toml::Table::from_iter([(
            "concurrency".to_owned(),
            toml::Value::Integer(i64::try_from(self.0.concurrency.get()).expect("copy concurrency is capped at 64")),
        )])
    }

    fn create(&self, app: &AppState) -> Result<Arc<dyn NodeJob>, String> {
        let Some(copier) = app.serving.cross_dc_copier().cloned() else {
            return Err("cross-datacenter copy is unavailable".to_owned());
        };
        Ok(Arc::new(DcCopyJob::new(copier, self.0)))
    }
}

struct PlacementReconcileFactory(PlacementReconcileParameters);

impl ScheduledJobFactory for PlacementReconcileFactory {
    fn kind(&self) -> &'static str {
        PLACEMENT_RECONCILE
    }

    fn settings(&self) -> toml::Table {
        toml::Table::new()
    }

    fn create(&self, app: &AppState) -> Result<Arc<dyn NodeJob>, String> {
        let Some(reconciler) = app.serving.placement_reconciler().cloned() else {
            return Err("placement reconciliation is unavailable".to_owned());
        };
        Ok(Arc::new(PlacementReconcileJob::new(reconciler, self.0)))
    }
}

struct ReclamationFactory(ReclamationParameters);

impl ScheduledJobFactory for ReclamationFactory {
    fn kind(&self) -> &'static str {
        RECLAMATION
    }

    fn settings(&self) -> toml::Table {
        toml::Table::new()
    }

    fn create(&self, app: &AppState) -> Result<Arc<dyn NodeJob>, String> {
        let Some(reclaimer) = app.serving.blob_reclaimer().cloned() else {
            return Err("reclamation is unavailable".to_owned());
        };
        Ok(Arc::new(ReclamationJob::new(reclaimer, self.0)))
    }
}

pub struct DcCopyJob {
    copier: Arc<dyn CrossDcCopier>,
    parameters: DcCopyParameters,
}

impl DcCopyJob {
    #[must_use]
    pub fn new(copier: Arc<dyn CrossDcCopier>, parameters: DcCopyParameters) -> Self {
        Self { copier, parameters }
    }
}

#[async_trait]
impl NodeJob for DcCopyJob {
    fn kind(&self) -> &'static str {
        DC_COPY
    }

    fn scope(&self) -> &'static str {
        ""
    }

    fn metadata(&self) -> NodeJobMetadata<'_> {
        NodeJobMetadata {
            lease_scope: LeaseScope::NodeLocal,
            repository: None,
            persist_as: None,
        }
    }

    async fn run(&self, context: &JobContext) -> Result<JobRunOutcome, JobFailure> {
        let cancellation = CancellationProbe::new(context);
        self.copier
            .copy_pass(&|| cancellation.is_cancelled(), self.parameters.concurrency())
            .await
            .map(|report| cancellation.outcome(report))
            .map_err(|error| task_failure(&error))
    }
}

pub struct PlacementReconcileJob {
    reconciler: Arc<dyn PlacementReconciler>,
    parameters: PlacementReconcileParameters,
}

impl PlacementReconcileJob {
    #[must_use]
    pub fn new(reconciler: Arc<dyn PlacementReconciler>, parameters: PlacementReconcileParameters) -> Self {
        Self { reconciler, parameters }
    }
}

#[async_trait]
impl NodeJob for PlacementReconcileJob {
    fn kind(&self) -> &'static str {
        PLACEMENT_RECONCILE
    }

    fn scope(&self) -> &'static str {
        ""
    }

    fn metadata(&self) -> NodeJobMetadata<'_> {
        NodeJobMetadata {
            lease_scope: LeaseScope::NodeLocal,
            repository: None,
            persist_as: None,
        }
    }

    async fn run(&self, context: &JobContext) -> Result<JobRunOutcome, JobFailure> {
        let cancellation = CancellationProbe::new(context);
        self.reconciler
            .reconcile_pass(&|| cancellation.is_cancelled(), self.parameters.batch)
            .await
            .map(|report| cancellation.outcome(report))
            .map_err(|error| task_failure(&error))
    }
}

pub struct ReclamationJob {
    reclaimer: Arc<dyn BlobReclaimer>,
    parameters: ReclamationParameters,
}

impl ReclamationJob {
    #[must_use]
    pub fn new(reclaimer: Arc<dyn BlobReclaimer>, parameters: ReclamationParameters) -> Self {
        Self { reclaimer, parameters }
    }
}

#[async_trait]
impl NodeJob for ReclamationJob {
    fn kind(&self) -> &'static str {
        RECLAMATION
    }

    fn scope(&self) -> &'static str {
        ""
    }

    fn metadata(&self) -> NodeJobMetadata<'_> {
        NodeJobMetadata {
            lease_scope: LeaseScope::ClusterSingleton(RECLAMATION.to_owned()),
            repository: None,
            persist_as: None,
        }
    }

    async fn run(&self, context: &JobContext) -> Result<JobRunOutcome, JobFailure> {
        let cancellation = CancellationProbe::new(context);
        self.reclaimer
            .reclaim_pass(
                &|| cancellation.is_cancelled(),
                context.authority_fence(),
                self.parameters.batch,
            )
            .await
            .map(|report| cancellation.outcome(report))
            .map_err(|error| task_failure(&error))
    }
}

const fn task_report(report: peryx_ha::AvailabilityTaskReport) -> JobReport {
    JobReport {
        processed: report.processed,
        changed: report.changed,
        quota_released: 0,
        quota_remaining: 0,
    }
}

fn task_failure(error: &peryx_ha::AvailabilityTaskError) -> JobFailure {
    JobFailure::new(error.code(), error.message())
}

#[cfg(test)]
#[path = "../../tests/unit/jobs_tests.rs"]
mod tests;
