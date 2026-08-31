//! Bounded node-local job scheduling.
//!
//! A [`NodeJob`] declares its [`kind`](NodeJob::kind), repository [`scope`](NodeJob::scope), and durable
//! record through [`persist_as`](NodeJob::persist_as). [`JobScheduler`] enforces global, per-kind, and
//! per-repository limits and rejects excess work instead of building an unbounded queue.
//!
//! The background maintenance the server runs on a timer - reclaim idle process resources, then
//! revalidate stale cached pages - is expressed as one maintenance job per installed ecosystem
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

use crate::serving::{CacheRefresher, IdleReclaimer, IntentFinalizer};
use crate::state::{AppState, ServingState};

pub use attempts::{CancelJobRun, JobAttemptControl};
pub use metrics::JobMetrics;
pub use metrics::Outcome as JobCompletionOutcome;
pub use scheduler::{JobLimits, JobScheduler, Submit};
pub use timer::{
    PluginScheduledJob, RegisteredScheduledJob, Schedule, ScheduledJob, ScheduledJobFactory, run_schedules,
};

/// How often the server runs a maintenance pass when node-local jobs are enabled.
pub const MAINTENANCE_INTERVAL: Duration = Duration::from_mins(1);

/// Documents a search rebuild commits per chunk by default, balancing commit overhead against the
/// writer memory held between commits.
pub const DEFAULT_SEARCH_REBUILD_CHUNK: usize = 1_000;
/// The CLI rejects larger chunks so one rebuild cannot buffer an unbounded batch before committing.
pub const MAX_SEARCH_REBUILD_CHUNK: usize = 1_000_000;

/// The counts a finished job reports, for its durable run record and lifecycle metrics.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct JobReport {
    /// Items the run examined.
    pub processed: u64,
    /// Items the run changed.
    pub changed: u64,
    /// Abandoned quota reservations the run released.
    pub quota_released: u64,
    /// Eligible quota reservations left for a later bounded pass.
    pub quota_remaining: u64,
}

/// The result of a job that stopped without failing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobRunOutcome {
    /// The job completed its work.
    Succeeded(JobReport),
    /// The job stopped after observing cancellation.
    Cancelled(JobReport),
}

impl JobRunOutcome {
    #[must_use]
    pub const fn succeeded(report: JobReport) -> Self {
        Self::Succeeded(report)
    }

    #[must_use]
    pub const fn cancelled(report: JobReport) -> Self {
        Self::Cancelled(report)
    }

    #[must_use]
    pub const fn report(self) -> JobReport {
        match self {
            Self::Succeeded(report) | Self::Cancelled(report) => report,
        }
    }
}

/// A node-local job lifecycle event published after metrics and durable history are final.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobCompletion {
    kind: &'static str,
    outcome: JobCompletionOutcome,
    report: Option<JobReport>,
}

impl JobCompletion {
    const fn new(kind: &'static str, outcome: JobCompletionOutcome, report: Option<JobReport>) -> Self {
        Self { kind, outcome, report }
    }

    #[must_use]
    pub const fn kind(self) -> &'static str {
        self.kind
    }

    #[must_use]
    pub const fn outcome(self) -> JobCompletionOutcome {
        self.outcome
    }

    #[must_use]
    pub const fn report(self) -> Option<JobReport> {
        self.report
    }
}

/// A bounded public failure category and message safe for durable history and operator responses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobFailure {
    code: &'static str,
    message: String,
}

impl JobFailure {
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

/// What a running job sees: the serving state to work over, a cooperative cancellation signal, and the
/// authority fence its writes carry.
pub struct JobContext {
    state: Arc<ServingState>,
    cancel: tokio_util::sync::CancellationToken,
    fence: u64,
}

impl JobContext {
    #[must_use]
    pub const fn state(&self) -> &Arc<ServingState> {
        &self.state
    }

    /// A job stamps this onto the records it writes so a later holder fences it out: if the authority
    /// transfers mid-run and its epoch advances, this run's writes carry the older epoch and lose to the
    /// new holder's. `0` for a node-wide job that names no repository, the closed sentinel the placement
    /// fence rejects.
    #[must_use]
    pub const fn authority_fence(&self) -> u64 {
        self.fence
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    #[must_use]
    pub fn now(&self) -> i64 {
        (self.state.clock)()
    }

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeJobMetadata<'a> {
    pub lease_scope: LeaseScope,
    pub repository: Option<&'a str>,
    pub persist_as: Option<JobKind>,
}

/// A unit of node-local maintenance the [`JobScheduler`] can run.
#[async_trait]
pub trait NodeJob: Send + Sync {
    fn kind(&self) -> &'static str;

    /// The repository or resource this run acts on. Two runs sharing a kind and scope conflict and
    /// never overlap; different scopes run concurrently. Empty names a node-wide task.
    fn scope(&self) -> &str;

    fn metadata(&self) -> NodeJobMetadata<'_>;

    fn lease_scope(&self) -> LeaseScope {
        self.metadata().lease_scope
    }

    fn repository(&self) -> Option<&str> {
        self.metadata().repository
    }

    fn persist_as(&self) -> Option<JobKind> {
        self.metadata().persist_as
    }

    /// # Errors
    /// Returns a user-visible message when the work fails.
    async fn run(&self, ctx: &JobContext) -> Result<JobRunOutcome, JobFailure>;
}

struct IdleReclaimJob {
    ecosystem: peryx_core::Ecosystem,
    reclaimer: Arc<dyn IdleReclaimer>,
}

const CACHE_MAINTENANCE: &str = "cache_maintenance";

#[async_trait]
impl NodeJob for IdleReclaimJob {
    fn kind(&self) -> &'static str {
        "idle_reclaim"
    }

    fn scope(&self) -> &str {
        self.ecosystem.as_str()
    }

    fn metadata(&self) -> NodeJobMetadata<'_> {
        NodeJobMetadata {
            lease_scope: LeaseScope::NodeLocal,
            repository: None,
            persist_as: Some(JobKind::new("idle_reclaim").expect("static job kind is valid")),
        }
    }

    async fn run(&self, ctx: &JobContext) -> Result<JobRunOutcome, JobFailure> {
        if ctx.is_cancelled() {
            return Ok(JobRunOutcome::cancelled(JobReport::default()));
        }
        let reclaimed = self.reclaimer.reclaim_idle(ctx.state().clone()).await;
        if reclaimed > 0 {
            tracing::info!(ecosystem = %self.ecosystem, reclaimed, "idle resources reclaimed");
        }
        let reclaimed = u64::try_from(reclaimed).expect("reclaimed count fits in u64");
        Ok(JobRunOutcome::succeeded(JobReport {
            processed: reclaimed,
            changed: reclaimed,
            ..JobReport::default()
        }))
    }
}

struct IntentFinalizeJob {
    ecosystem: peryx_core::Ecosystem,
    finalizer: Arc<dyn IntentFinalizer>,
}

#[async_trait]
impl NodeJob for IntentFinalizeJob {
    fn kind(&self) -> &'static str {
        "intent_finalize"
    }

    fn scope(&self) -> &str {
        self.ecosystem.as_str()
    }

    fn metadata(&self) -> NodeJobMetadata<'_> {
        NodeJobMetadata {
            lease_scope: LeaseScope::NodeLocal,
            repository: None,
            persist_as: Some(JobKind::new("intent_finalize").expect("static job kind is valid")),
        }
    }

    async fn run(&self, ctx: &JobContext) -> Result<JobRunOutcome, JobFailure> {
        if ctx.is_cancelled() {
            return Ok(JobRunOutcome::cancelled(JobReport::default()));
        }
        let finalized = self.finalizer.finalize_admitted(ctx.state().clone()).await;
        if finalized > 0 {
            tracing::info!(ecosystem = %self.ecosystem, finalized, "admitted writes finalized at home");
        }
        Ok(JobRunOutcome::succeeded(JobReport {
            processed: finalized,
            changed: finalized,
            ..JobReport::default()
        }))
    }
}

struct CacheRefreshJob {
    ecosystem: peryx_core::Ecosystem,
    refresher: Arc<dyn CacheRefresher>,
}

#[async_trait]
impl NodeJob for CacheRefreshJob {
    fn kind(&self) -> &'static str {
        "cache_refresh"
    }

    fn scope(&self) -> &str {
        self.ecosystem.as_str()
    }

    fn metadata(&self) -> NodeJobMetadata<'_> {
        NodeJobMetadata {
            lease_scope: LeaseScope::NodeLocal,
            repository: None,
            persist_as: Some(JobKind::new("cache_refresh").expect("static job kind is valid")),
        }
    }

    async fn run(&self, ctx: &JobContext) -> Result<JobRunOutcome, JobFailure> {
        if ctx.is_cancelled() {
            return Ok(JobRunOutcome::cancelled(JobReport::default()));
        }
        let sweep = self
            .refresher
            .refresh_stale(ctx.state().clone())
            .await
            .map_err(|message| JobFailure::new("cache_refresh", message))?;
        if sweep.checked > 0 {
            tracing::info!(ecosystem = %self.ecosystem, ?sweep, "background refresh sweep");
        }
        Ok(JobRunOutcome::succeeded(JobReport {
            processed: sweep.checked as u64,
            changed: sweep.changed as u64,
            ..JobReport::default()
        }))
    }
}

const MAX_JOB_RUNS: usize = 10_000;

pub(super) struct JobHistoryCleanup {
    retain: usize,
}

impl JobHistoryCleanup {
    const fn retaining(retain: usize) -> Self {
        Self { retain }
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

    fn metadata(&self) -> NodeJobMetadata<'_> {
        NodeJobMetadata {
            lease_scope: LeaseScope::NodeLocal,
            repository: None,
            persist_as: None,
        }
    }

    async fn run(&self, ctx: &JobContext) -> Result<JobRunOutcome, JobFailure> {
        let mut removed = 0_u64;
        loop {
            if ctx.is_cancelled() {
                return Ok(JobRunOutcome::cancelled(JobReport {
                    processed: removed,
                    changed: removed,
                    ..JobReport::default()
                }));
            }
            let batch = ctx
                .state()
                .meta
                .prune_job_runs_batch(self.retain)
                .map_err(|error| JobFailure::new("storage", error.to_string()))?;
            removed += u64::try_from(batch).expect("bounded batch fits in u64");
            if batch == 0 {
                return Ok(JobRunOutcome::succeeded(JobReport {
                    processed: removed,
                    changed: removed,
                    ..JobReport::default()
                }));
            }
        }
    }
}

/// How long a settled ingress intent is kept before the reaper prunes it, so a brief home-DC
/// finalization lag never drops an intent a slow duplicate retry still resolves against.
pub const INGRESS_INTENT_RETENTION_SECS: i64 = 3600;
/// How long a pending ingress intent may hold its authority's admission capacity once the owning
/// ecosystem has given up on finalizing it.
///
/// It bounds the one failure no request-side code can clean up: a client that hangs up mid-store drops
/// the request future outright, so nothing runs to release the intent. It is set well past any single
/// upload's storing time, and expiry additionally requires the ecosystem's own evidence that nothing
/// finalizable was ever stored.
pub const INGRESS_STAGING_DEADLINE_SECS: i64 = 3600;
/// Finalization refusals after which an ecosystem's sweep stops offering a pending intent.
///
/// Repeating the attempt leaves a node that crashed between storing an upload's rows and advancing its
/// intent room to recover on a later tick, while still bounding how long an unfinalizable head occupies
/// a sweep batch.
pub const MAX_INTENT_REFUSALS: u32 = 3;
/// Pending quota owners get this long to finish before node maintenance may reclaim their reservation.
pub const QUOTA_RESERVATION_GRACE_SECS: i64 = 3600;
/// Batching caps the metadata write set during startup and recurring repair.
pub const QUOTA_REPAIR_BATCH: usize = 128;

/// Bound the two write-idempotency ledgers so neither grows without end.
///
/// A hosted write stages a durable ingress intent and claims an operation id before it mutates; a retry
/// replays both instead of remutating. Left alone each row would live forever: the ingress-intent table
/// would fill to its admission cap and refuse new writes, and the operation-outcome ledger would keep
/// every terminal result. This drains the settled rows of both - an [`Admitted`] or [`Expired`] intent
/// past its retention, and a terminal operation past its own expiry - so the idempotency guarantees stay
/// bounded.
///
/// A [`Pending`] intent whose write may still finalize is never dropped. One the owning ecosystem has
/// repeatedly refused, because nothing it could finalize was ever stored, is a different row: it can
/// only have come from a write that died before storing anything, most often a client that hung up
/// mid-store and dropped the request future before any release could run. Those advance to [`Expired`]
/// past their staging deadline, which is what returns the authority's admission capacity.
///
/// [`Admitted`]: peryx_storage::meta::IntentPhase::Admitted
/// [`Expired`]: peryx_storage::meta::IntentPhase::Expired
/// [`Pending`]: peryx_storage::meta::IntentPhase::Pending
pub(super) struct WriteLedgerReap {
    batch: usize,
}

impl Default for WriteLedgerReap {
    fn default() -> Self {
        Self {
            batch: QUOTA_REPAIR_BATCH,
        }
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

    fn metadata(&self) -> NodeJobMetadata<'_> {
        NodeJobMetadata {
            lease_scope: LeaseScope::NodeLocal,
            repository: None,
            persist_as: Some(JobKind::new("write_ledger_reap").expect("static job kind is valid")),
        }
    }

    async fn run(&self, ctx: &JobContext) -> Result<JobRunOutcome, JobFailure> {
        let mut reaped = 0_u64;
        loop {
            if ctx.is_cancelled() {
                return Ok(JobRunOutcome::cancelled(JobReport {
                    processed: reaped,
                    changed: reaped,
                    ..JobReport::default()
                }));
            }
            let now = (ctx.state().clock)();
            let expired = reap_storage_result(ctx.state().meta.expire_stale_intents(
                now,
                INGRESS_STAGING_DEADLINE_SECS,
                MAX_INTENT_REFUSALS,
                self.batch,
            ))?;
            let intents = reap_storage_result(ctx.state().meta.prune_ingress_intents(
                now,
                INGRESS_INTENT_RETENTION_SECS,
                self.batch,
            ))?;
            let outcomes = reap_storage_result(ctx.state().meta.prune_operation_outcomes(now, self.batch))?;
            reaped += reaped_count(expired) + reaped_count(intents) + reaped_count(outcomes);
            if expired == 0 && intents == 0 && outcomes == 0 {
                break;
            }
        }
        let quota = reap_storage_result(ctx.state().meta.repair_abandoned_quota_reservations(
            (ctx.state().clock)().saturating_sub(QUOTA_RESERVATION_GRACE_SECS),
            self.batch,
        ))?;
        let quota_released = reaped_count(quota.released);
        let quota_remaining = reaped_count(quota.remaining);
        Ok(JobRunOutcome::succeeded(JobReport {
            processed: reaped + quota_released + quota_remaining,
            changed: reaped + quota_released,
            quota_released,
            quota_remaining,
        }))
    }
}

fn reaped_count(batch: usize) -> u64 {
    u64::try_from(batch).expect("bounded batch fits in u64")
}

fn reap_storage_result<T, E: std::fmt::Display>(result: Result<T, E>) -> Result<T, JobFailure> {
    result.map_err(|error| JobFailure::new("storage", error.to_string()))
}

const SEARCH_REBUILD: &str = "search_rebuild";

/// Rebuild the node's derived resource search index from authoritative metadata.
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

    fn metadata(&self) -> NodeJobMetadata<'_> {
        NodeJobMetadata {
            lease_scope: LeaseScope::NodeLocal,
            repository: None,
            persist_as: Some(JobKind::new("search_rebuild").expect("static job kind is valid")),
        }
    }

    async fn run(&self, ctx: &JobContext) -> Result<JobRunOutcome, JobFailure> {
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
            RebuildOutcome::Published { documents } => JobRunOutcome::succeeded(JobReport {
                processed: documents,
                changed: documents,
                ..JobReport::default()
            }),
            RebuildOutcome::Aborted { documents } => JobRunOutcome::cancelled(JobReport {
                processed: documents,
                changed: 0,
                ..JobReport::default()
            }),
        })
    }
}

pub fn submit_maintenance(app: &AppState, scheduler: &JobScheduler) {
    for (ecosystem, reclaimer) in app.idle_reclaimers() {
        scheduler.submit(Arc::new(IdleReclaimJob {
            ecosystem: ecosystem.clone(),
            reclaimer: reclaimer.clone(),
        }));
    }
    for (ecosystem, finalizer) in app.intent_finalizers() {
        scheduler.submit(Arc::new(IntentFinalizeJob {
            ecosystem: ecosystem.clone(),
            finalizer: finalizer.clone(),
        }));
    }
    for (ecosystem, refresher) in app.cache_refreshers() {
        scheduler.submit(Arc::new(CacheRefreshJob {
            ecosystem: ecosystem.clone(),
            refresher: refresher.clone(),
        }));
    }
}

/// # Errors
/// Returns a user-visible message when no installed driver supports the kind or its parameters do
/// not resolve against the runtime state.
pub fn scheduled_job(app: &AppState, job: &ScheduledJob) -> Result<Arc<dyn NodeJob>, String> {
    match job {
        ScheduledJob::Plugin(job) => job.factory.create(app),
        ScheduledJob::Registered(job) => job.factory.create(app),
        ScheduledJob::CacheMaintenance => Err("cache maintenance expands through installed drivers".to_owned()),
    }
}
