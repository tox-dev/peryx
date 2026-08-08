//! Typed node-local jobs: a registry-free scheduler for a single node's maintenance work.
//!
//! A [`NodeJob`] declares its low-cardinality [`kind`](NodeJob::kind), the repository
//! [`scope`](NodeJob::scope) it acts on (its conflict key within a kind), and whether it records a
//! durable run through [`persist_as`](NodeJob::persist_as). The [`JobScheduler`] runs jobs on the
//! Tokio runtime under global, per-kind, and per-repository bounds, hands each a [`JobContext`] that
//! carries the serving state and a cancellation signal, and refuses overlapping or excess work rather
//! than queueing it unbounded.
//!
//! The background maintenance the server runs on a timer - reclaim idle process resources, then
//! revalidate stale cached pages - is expressed as one [`MaintenanceJob`] per installed ecosystem
//! driver, so independent ecosystems sweep concurrently while a single ecosystem never sweeps itself
//! twice at once.

mod attempts;
mod metrics;
mod scheduler;
mod timer;

#[cfg(test)]
#[path = "../../tests/unit/jobs/tests.rs"]
mod tests;

use std::num::NonZeroUsize;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use peryx_search::{RebuildOutcome, RebuildProgress};
use peryx_storage::meta::JobKind;

use crate::serving::MaintenanceDriver;
use crate::state::{AppState, ServingState};

pub use attempts::{CancelJobRun, JobAttemptControl};
pub use metrics::JobMetrics;
pub use scheduler::{JobLimits, JobScheduler, Submit};
pub use timer::{Schedule, ScheduledJob, run_schedules};

/// How often the server runs a maintenance pass when node-local jobs are enabled.
pub const MAINTENANCE_INTERVAL: Duration = Duration::from_mins(1);

/// Default projects admitted by one catalog-sync run.
pub const DEFAULT_CATALOG_PROJECTS: usize = 10_000;
/// Default concurrent project-metadata requests in one catalog-sync run.
pub const DEFAULT_CATALOG_CONCURRENCY: usize = 4;
/// Default wall-time budget for one catalog-sync run.
pub const DEFAULT_CATALOG_TIMEOUT: Duration = Duration::from_mins(15);
/// Startup rejects larger project budgets to keep each run's memory and request count bounded.
pub const MAX_CATALOG_PROJECTS_PER_RUN: usize = 100_000;
/// Startup rejects larger request pools to protect the upstream and foreground traffic.
pub const MAX_CATALOG_CONCURRENCY: usize = 32;
/// Startup rejects longer runs so a stuck source cannot occupy a worker indefinitely.
pub const MAX_CATALOG_TIMEOUT: Duration = Duration::from_hours(24);

/// Documents a search rebuild commits per chunk by default, balancing commit overhead against the
/// writer memory held between commits.
pub const DEFAULT_SEARCH_REBUILD_CHUNK: usize = 1_000;
/// The CLI rejects larger chunks so one rebuild cannot buffer an unbounded batch before committing.
pub const MAX_SEARCH_REBUILD_CHUNK: usize = 1_000_000;

/// The source, repository, and work limits shared by scheduled and one-shot catalog syncs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogSyncParameters {
    pub repository: String,
    pub source: Option<String>,
    pub max_projects: NonZeroUsize,
    pub concurrency: NonZeroUsize,
    pub timeout: Duration,
}

impl CatalogSyncParameters {
    /// Build the default bounded work budget for `repository`.
    #[must_use]
    pub fn new(repository: impl Into<String>) -> Self {
        Self {
            repository: repository.into(),
            source: None,
            max_projects: NonZeroUsize::MIN.saturating_add(DEFAULT_CATALOG_PROJECTS - 1),
            concurrency: NonZeroUsize::MIN.saturating_add(DEFAULT_CATALOG_CONCURRENCY - 1),
            timeout: DEFAULT_CATALOG_TIMEOUT,
        }
    }
}

/// The counts a finished job reports, for its durable run record and lifecycle metrics.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct JobReport {
    /// Items the run examined.
    pub processed: u64,
    /// Items the run changed.
    pub changed: u64,
}

/// A bounded public failure category and message safe for durable history and operator responses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobFailure {
    code: &'static str,
    message: String,
}

impl JobFailure {
    /// Build a failure from a stable category and caller-sanitized message.
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Split the failure for transfer across service boundaries.
    #[must_use]
    pub fn into_parts(self) -> (&'static str, String) {
        (self.code, self.message)
    }
}

impl std::fmt::Display for JobFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for JobFailure {}

impl From<peryx_ha::AvailabilityTaskReport> for JobReport {
    fn from(report: peryx_ha::AvailabilityTaskReport) -> Self {
        Self {
            processed: report.processed,
            changed: report.changed,
        }
    }
}

impl From<peryx_ha::AvailabilityTaskError> for JobFailure {
    fn from(error: peryx_ha::AvailabilityTaskError) -> Self {
        Self::new(error.code(), error.message())
    }
}

/// What a running job sees: the serving state to work over, a cooperative cancellation signal, and the
/// authority fence its writes carry.
pub struct JobContext {
    state: Arc<ServingState>,
    cancel: tokio_util::sync::CancellationToken,
    fence: u64,
}

impl JobContext {
    /// The serving state: stores, caches, and configured indexes the job acts on.
    #[must_use]
    pub const fn state(&self) -> &Arc<ServingState> {
        &self.state
    }

    /// The committed authority epoch for this job's repository, snapshotted when the lease was taken.
    ///
    /// A job stamps this onto the records it writes so a later holder fences it out: if the authority
    /// transfers mid-run and its epoch advances, this run's writes carry the older epoch and lose to the
    /// new holder's. `0` for a node-wide job that names no repository, the closed sentinel the placement
    /// fence rejects.
    #[must_use]
    pub const fn authority_fence(&self) -> u64 {
        self.fence
    }

    /// Whether shutdown has asked this job to stop; a cooperative job polls it between units of work.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Resolves once cancellation is requested, to `select!` a long wait against shutdown.
    pub async fn cancelled(&self) {
        self.cancel.cancelled().await;
    }
}

/// How a job's ownership is scoped across the cluster, deciding whether the runner takes a
/// control-plane lease before it runs and which epoch fences its writes.
///
/// A per-repository job is already fenced by its authority epoch (see [`NodeJob::repository`]); this is
/// the orthogonal control for the node-wide jobs a repository epoch does not cover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseScope {
    /// The job runs on every node independently: cache maintenance, search rebuilds, a per-repository
    /// job the authority epoch already fences. The runner takes no control-plane lease and makes no
    /// control-plane call, so a node-local kind never depends on the ownership group.
    NodeLocal,
    /// Exactly one holder across the cluster runs the job at a time, keyed by `key`. The runner claims a
    /// lease under the ownership group's monotonic term before the run, stamps that term as the run's
    /// fence, and releases the lease after: a partitioned former holder mints a stale term and loses the
    /// claim, and a holder the group superseded mid-run has its write fenced.
    ClusterSingleton(String),
}

/// A unit of node-local maintenance the [`JobScheduler`] can run.
#[async_trait]
pub trait NodeJob: Send + Sync {
    /// A stable, low-cardinality label for this job's kind, used for metrics and conflict keys.
    fn kind(&self) -> &'static str;

    /// The repository or resource this run acts on. Two runs sharing a kind and scope conflict and
    /// never overlap; different scopes run concurrently. Empty names a node-wide task.
    fn scope(&self) -> &str;

    /// How this job's ownership is scoped across the cluster. The default [`LeaseScope::NodeLocal`] runs
    /// the job on every node with no control-plane lease; a job that must run on one node at a time
    /// returns [`LeaseScope::ClusterSingleton`] so the runner fences it through a control-plane lease.
    fn lease_scope(&self) -> LeaseScope {
        LeaseScope::NodeLocal
    }

    /// The configured repository authorizing this attempt, when the scope names one repository.
    fn repository(&self) -> Option<&str> {
        None
    }

    /// The durable job kind to record a run under, or `None` to run without a persisted history entry.
    fn persist_as(&self) -> Option<JobKind> {
        None
    }

    /// Do the work. A cooperative job polls `ctx` for cancellation and returns early when asked.
    ///
    /// # Errors
    /// Returns a user-visible message when the work fails.
    async fn run(&self, ctx: &JobContext) -> Result<JobReport, JobFailure>;
}

/// The server's maintenance pass for one ecosystem: reclaim expired process-local resources, then
/// revalidate that ecosystem's stale cached pages. Reclaim runs first so an upstream stall during the
/// refresh cannot extend an idle resource's deadline.
struct MaintenanceJob {
    driver: Arc<dyn MaintenanceDriver>,
}

const CACHE_MAINTENANCE: &str = "cache_maintenance";

#[async_trait]
impl NodeJob for MaintenanceJob {
    fn kind(&self) -> &'static str {
        CACHE_MAINTENANCE
    }

    fn scope(&self) -> &str {
        self.driver.ecosystem().as_str()
    }

    fn persist_as(&self) -> Option<JobKind> {
        Some(JobKind::CacheRefresh)
    }

    async fn run(&self, ctx: &JobContext) -> Result<JobReport, JobFailure> {
        let ecosystem = self.driver.ecosystem();
        if let Some(reclaimer) = self.driver.maintenance_capabilities().idle_reclaimer {
            let reclaimed = reclaimer.reclaim_idle(ctx.state().clone()).await;
            if reclaimed > 0 {
                tracing::info!(ecosystem = %ecosystem, reclaimed, "idle resources reclaimed");
            }
        }
        if ctx.is_cancelled() {
            return Ok(JobReport::default());
        }
        if let Some(finalizer) = self.driver.maintenance_capabilities().intent_finalizer {
            let finalized = finalizer.finalize_admitted(ctx.state().clone()).await;
            if finalized > 0 {
                tracing::info!(ecosystem = %ecosystem, finalized, "admitted uploads finalized at home");
            }
        }
        if ctx.is_cancelled() {
            return Ok(JobReport::default());
        }
        let Some(refresher) = self.driver.maintenance_capabilities().cache_refresher else {
            return Ok(JobReport::default());
        };
        let sweep = refresher
            .refresh_stale(ctx.state().clone())
            .await
            .map_err(|message| JobFailure::new("cache_refresh", message))?;
        if sweep.checked > 0 {
            tracing::info!(ecosystem = %ecosystem, ?sweep, "background refresh sweep");
        }
        Ok(JobReport {
            processed: sweep.checked as u64,
            changed: sweep.changed as u64,
        })
    }
}

const MAX_JOB_RUNS: usize = 10_000;

pub(super) struct JobHistoryCleanup {
    retain: usize,
}

impl Default for JobHistoryCleanup {
    fn default() -> Self {
        Self { retain: MAX_JOB_RUNS }
    }
}

#[async_trait]
impl NodeJob for JobHistoryCleanup {
    fn kind(&self) -> &'static str {
        "job_history_cleanup"
    }

    fn scope(&self) -> &'static str {
        ""
    }

    async fn run(&self, ctx: &JobContext) -> Result<JobReport, JobFailure> {
        let mut removed = 0_u64;
        loop {
            if ctx.is_cancelled() {
                return Ok(JobReport {
                    processed: removed,
                    changed: removed,
                });
            }
            let batch = ctx
                .state()
                .meta
                .prune_job_runs_batch(self.retain)
                .map_err(|error| JobFailure::new("storage", error.to_string()))?;
            removed += u64::try_from(batch).expect("bounded batch fits in u64");
            if batch == 0 {
                return Ok(JobReport {
                    processed: removed,
                    changed: removed,
                });
            }
        }
    }
}

/// How long a settled ingress intent is kept before the reaper prunes it, so a brief home-DC
/// finalization lag never drops an intent a slow duplicate retry still resolves against.
pub const INGRESS_INTENT_RETENTION_SECS: i64 = 3600;

/// Bound the two write-idempotency ledgers so neither grows without end.
///
/// A hosted write stages a durable ingress intent and claims an operation id before it mutates; a retry
/// replays both instead of remutating. Left alone each row would live forever: the ingress-intent table
/// would fill to its admission cap and refuse new uploads, and the operation-outcome ledger would keep
/// every terminal result. This drains the settled rows of both - an [`Admitted`] or [`Expired`] intent
/// past its retention, and a terminal operation past its own expiry - so the idempotency guarantees stay
/// bounded. A [`Pending`] intent is never dropped, since its write may still finalize.
///
/// [`Admitted`]: peryx_storage::meta::IntentPhase::Admitted
/// [`Expired`]: peryx_storage::meta::IntentPhase::Expired
/// [`Pending`]: peryx_storage::meta::IntentPhase::Pending
pub(super) struct WriteLedgerReap {
    batch: usize,
}

impl Default for WriteLedgerReap {
    fn default() -> Self {
        Self { batch: MAX_JOB_RUNS }
    }
}

#[async_trait]
impl NodeJob for WriteLedgerReap {
    fn kind(&self) -> &'static str {
        "write_ledger_reap"
    }

    fn scope(&self) -> &'static str {
        ""
    }

    async fn run(&self, ctx: &JobContext) -> Result<JobReport, JobFailure> {
        let mut reaped = 0_u64;
        loop {
            if ctx.is_cancelled() {
                break;
            }
            let now = (ctx.state().clock)();
            let intents = ctx
                .state()
                .meta
                .prune_ingress_intents(now, INGRESS_INTENT_RETENTION_SECS, self.batch)
                .map_err(reap_storage_failure)?;
            let outcomes = ctx
                .state()
                .meta
                .prune_operation_outcomes(now, self.batch)
                .map_err(reap_storage_failure)?;
            reaped += reaped_count(intents) + reaped_count(outcomes);
            if intents == 0 && outcomes == 0 {
                break;
            }
        }
        Ok(JobReport {
            processed: reaped,
            changed: reaped,
        })
    }
}

/// One reaped batch as a report count.
fn reaped_count(batch: usize) -> u64 {
    u64::try_from(batch).expect("bounded batch fits in u64")
}

/// Surface a ledger prune failure as a storage job failure.
#[expect(
    clippy::needless_pass_by_value,
    reason = "map_err hands the mapper an owned error; the message only needs to borrow it"
)]
fn reap_storage_failure(error: peryx_storage::meta::MetaError) -> JobFailure {
    JobFailure::new("storage", error.to_string())
}

const SEARCH_REBUILD: &str = "search_rebuild";

/// Rebuild the node's derived package search index from authoritative metadata.
///
/// The index is ecosystem-neutral, so this job is too: it drives every installed ecosystem's indexer
/// through the shared engine rather than acting on one ecosystem's store. It is an operator recovery
/// path for when incremental refresh cannot bring the index current. The engine publishes the rebuilt
/// index atomically, so searches keep serving the prior complete index until the rebuild finishes; a
/// run cancelled at shutdown leaves the served index untouched.
pub struct SearchRebuildJob {
    chunk: NonZeroUsize,
}

impl SearchRebuildJob {
    /// Rebuild the index committing `chunk` documents at a time.
    #[must_use]
    pub const fn new(chunk: NonZeroUsize) -> Self {
        Self { chunk }
    }
}

#[async_trait]
impl NodeJob for SearchRebuildJob {
    fn kind(&self) -> &'static str {
        SEARCH_REBUILD
    }

    fn scope(&self) -> &'static str {
        ""
    }

    fn persist_as(&self) -> Option<JobKind> {
        Some(JobKind::SearchRebuild)
    }

    async fn run(&self, ctx: &JobContext) -> Result<JobReport, JobFailure> {
        let state = ctx.state();
        let mut reported = 0_u64;
        let outcome = state
            .search
            .rebuild(&state.indexer_ctx(), self.chunk, &mut |progress: RebuildProgress| {
                if ctx.is_cancelled() {
                    return ControlFlow::Break(());
                }
                if progress.indexed != reported {
                    reported = progress.indexed;
                    tracing::info!(
                        indexed = progress.indexed,
                        total = progress.total,
                        "search rebuild progress"
                    );
                }
                ControlFlow::Continue(())
            })
            .map_err(|error| JobFailure::new("search_rebuild", error.to_string()))?;
        Ok(match outcome {
            RebuildOutcome::Published { documents } => JobReport {
                processed: documents,
                changed: documents,
            },
            RebuildOutcome::Aborted { documents } => JobReport {
                processed: documents,
                changed: 0,
            },
        })
    }
}

const DC_COPY: &str = "dc_copy";

/// Default copies a cross-data-center pass runs at once, bounding peer fetches and target writes in flight.
pub const DEFAULT_DC_COPY_CONCURRENCY: usize = 8;
/// Startup rejects a larger fan-out so one pass cannot open an unbounded number of peer transfers.
pub const MAX_DC_COPY_CONCURRENCY: usize = 64;

/// The bounds one scheduled cross-data-center copy pass runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DcCopyParameters {
    /// Copies to run at once across the backlog a single pass drains.
    pub concurrency: NonZeroUsize,
}

impl DcCopyParameters {
    /// The default bound: a handful of concurrent copies.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            concurrency: NonZeroUsize::MIN.saturating_add(DEFAULT_DC_COPY_CONCURRENCY - 1),
        }
    }
}

impl Default for DcCopyParameters {
    fn default() -> Self {
        Self::new()
    }
}

/// The cross-data-center blob copier a node registers to back its scheduled [`DcCopy`](ScheduledJob::DcCopy)
/// pass.
///
/// The copy pulls verified bytes from a peer over the replication blob transport, a network dependency
/// this neutral crate does not carry, so the binary implements the copier and registers it on the
/// [`ServingState`], exactly as it registers the [`OwnershipAuthority`](crate::state::OwnershipAuthority).
/// A process that registers none - single node, no roster, or an object-store backend - runs the job as a
/// no-op.
pub use peryx_ha::CrossDcCopier;

/// The node-wide job that runs one cross-data-center blob copy pass through the registered
/// [`CrossDcCopier`].
///
/// A process that registered none does nothing, so an unconfigured node pays only the scheduler tick.
pub struct DcCopyJob {
    parameters: DcCopyParameters,
}

#[async_trait]
impl NodeJob for DcCopyJob {
    fn kind(&self) -> &'static str {
        DC_COPY
    }

    fn scope(&self) -> &'static str {
        ""
    }

    async fn run(&self, ctx: &JobContext) -> Result<JobReport, JobFailure> {
        match ctx.state().cross_dc_copier() {
            Some(copier) => copier
                .copy_pass(&|| ctx.is_cancelled(), self.parameters.concurrency)
                .await
                .map(Into::into)
                .map_err(Into::into),
            None => Ok(JobReport::default()),
        }
    }
}

/// Submit one node-wide cross-data-center blob copy pass. The scheduler drops it when a prior pass is
/// still draining the backlog, so a slow transfer never stacks passes.
pub fn submit_dc_copy(scheduler: &JobScheduler, parameters: DcCopyParameters) {
    scheduler.submit(Arc::new(DcCopyJob { parameters }));
}

/// The node-wide job kind label for the placement-reconciliation pass.
const PLACEMENT_RECONCILE: &str = "placement_reconcile";

/// Default distinct digests one reconciliation pass classifies per ledger page before it resumes past
/// its cursor. The pass loops until the ledger is drained, so this bounds one page's read, not the pass.
pub const DEFAULT_PLACEMENT_RECONCILE_BATCH: usize = 256;

/// The bounds one scheduled placement-reconciliation pass runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlacementReconcileParameters {
    /// Distinct digests classified per ledger page a single pass reads.
    pub batch: NonZeroUsize,
}

impl PlacementReconcileParameters {
    /// The default bound: a few hundred digests per page.
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

/// The placement reconciler a node registers to back its scheduled placement-reconciliation pass.
///
/// Reconciliation compares the placement ledger against the replication policy - which data centers
/// should hold each digest - read from configuration this neutral crate does not carry, so the binary
/// implements the reconciler and registers it on the [`ServingState`], exactly as it registers the
/// [`CrossDcCopier`]. A process that registers none - a single data center, or no membership - runs the
/// job as a no-op.
pub use peryx_ha::PlacementReconciler;

/// The node-wide job that runs one placement-reconciliation pass through the registered
/// [`PlacementReconciler`].
///
/// A process that registered none does nothing, so an unconfigured node pays only the scheduler tick.
pub struct PlacementReconcileJob {
    parameters: PlacementReconcileParameters,
}

#[async_trait]
impl NodeJob for PlacementReconcileJob {
    fn kind(&self) -> &'static str {
        PLACEMENT_RECONCILE
    }

    fn scope(&self) -> &'static str {
        ""
    }

    async fn run(&self, ctx: &JobContext) -> Result<JobReport, JobFailure> {
        match ctx.state().placement_reconciler() {
            Some(reconciler) => reconciler
                .reconcile_pass(&|| ctx.is_cancelled(), self.parameters.batch)
                .await
                .map(Into::into)
                .map_err(Into::into),
            None => Ok(JobReport::default()),
        }
    }
}

/// Submit one node-wide placement-reconciliation pass. The scheduler drops it when a prior pass is still
/// draining the ledger, so a slow pass never stacks.
pub fn submit_placement_reconcile(scheduler: &JobScheduler, parameters: PlacementReconcileParameters) {
    scheduler.submit(Arc::new(PlacementReconcileJob { parameters }));
}

const RECLAMATION: &str = "reclamation";

/// Default candidate digests one reclamation pass scans before it yields the scheduler.
pub const DEFAULT_RECLAMATION_BATCH: usize = 256;

/// The bounds one scheduled reclamation-selection pass runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReclamationParameters {
    /// Candidate digests a single pass scans before it yields, bounding the ledger it reads per pass.
    pub batch: NonZeroUsize,
}

impl ReclamationParameters {
    /// The default bound: a few hundred candidates per pass.
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

/// The blob-reclamation selector a node registers to back its scheduled [`Reclamation`](ScheduledJob::Reclamation)
/// pass.
///
/// Selecting a digest safe to reclaim reads the reference inventory across every ecosystem driver and the
/// observed replication frontiers - dependencies this neutral crate does not carry - so the binary
/// implements the selector and registers it on the [`ServingState`], exactly as it registers the
/// [`CrossDcCopier`]. A process that registers none - single node, or no data-center membership - runs the
/// job as a no-op. The pass records reclamation tombstones and never deletes bytes.
pub use peryx_ha::BlobReclaimer;

/// The cluster-singleton job that runs one blob-reclamation selection pass through the registered
/// [`BlobReclaimer`].
///
/// Reclamation decides destructive storage state, so exactly one node runs it at a time cluster-wide: the
/// scheduler fences it under the ownership group's monotonic term through
/// [`LeaseScope::ClusterSingleton`], and a superseded holder's writes are rejected. A process that
/// registered no reclaimer does nothing, so an unconfigured node pays only the lease claim and the tick.
pub struct ReclamationJob {
    parameters: ReclamationParameters,
}

#[async_trait]
impl NodeJob for ReclamationJob {
    fn kind(&self) -> &'static str {
        RECLAMATION
    }

    fn scope(&self) -> &'static str {
        ""
    }

    fn lease_scope(&self) -> LeaseScope {
        LeaseScope::ClusterSingleton(RECLAMATION.to_owned())
    }

    async fn run(&self, ctx: &JobContext) -> Result<JobReport, JobFailure> {
        match ctx.state().blob_reclaimer() {
            Some(reclaimer) => reclaimer
                .reclaim_pass(&|| ctx.is_cancelled(), ctx.authority_fence(), self.parameters.batch)
                .await
                .map(Into::into)
                .map_err(Into::into),
            None => Ok(JobReport::default()),
        }
    }
}

/// Submit one cluster-singleton blob-reclamation selection pass. The scheduler drops it when a prior pass
/// is still draining the ledger, so a slow pass never stacks.
pub fn submit_reclamation(scheduler: &JobScheduler, parameters: ReclamationParameters) {
    scheduler.submit(Arc::new(ReclamationJob { parameters }));
}

/// Submit one maintenance job per installed ecosystem driver. The scheduler runs them concurrently
/// across ecosystems under its bounds and drops any whose predecessor is still sweeping.
pub fn submit_maintenance(app: &AppState, scheduler: &JobScheduler) {
    for driver in app.maintenance_drivers() {
        scheduler.submit(Arc::new(MaintenanceJob { driver: driver.clone() }));
    }
}

/// Resolve one configured job through the installed ecosystem drivers.
///
/// # Errors
/// Returns a user-visible message when no installed driver supports the kind or its parameters do
/// not resolve against the runtime state.
pub fn scheduled_job(app: &AppState, job: &ScheduledJob) -> Result<Arc<dyn NodeJob>, String> {
    app.drivers()
        .filter_map(|driver| driver.capabilities().jobs)
        .find_map(|driver| driver.node_job(job))
        .unwrap_or_else(|| Err(format!("no installed ecosystem supports {}", job.as_str())))
}
