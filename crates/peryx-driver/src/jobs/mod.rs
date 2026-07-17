//! Typed node-local jobs: a registry-free scheduler for a single node's maintenance work.
//!
//! A [`NodeJob`] declares its low-cardinality [`kind`](NodeJob::kind), the repository
//! [`scope`](NodeJob::scope) it acts on (its conflict key within a kind), and whether it records a
//! durable run through [`persist_as`](NodeJob::persist_as). The [`JobScheduler`] runs jobs on the
//! Tokio runtime under global, per-kind, and per-repository bounds, hands each a [`JobContext`] that
//! carries the serving state and a cancellation signal, and refuses overlapping or excess work rather
//! than queueing it unbounded.
//!
//! The background maintenance the server runs on a timer — reclaim idle process resources, then
//! revalidate stale cached pages — is expressed as one [`MaintenanceJob`] per installed ecosystem
//! driver, so independent ecosystems sweep concurrently while a single ecosystem never sweeps itself
//! twice at once.

mod metrics;
mod scheduler;

#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use peryx_storage::meta::JobKind;

use crate::serving::EcosystemDriver;
use crate::state::{AppState, ServingState};

pub use metrics::JobMetrics;
pub use scheduler::{JobLimits, JobScheduler, Submit};

/// How often the server runs a maintenance pass when node-local jobs are enabled.
pub const MAINTENANCE_INTERVAL: Duration = Duration::from_mins(1);

/// The counts a finished job reports, for its durable run record and lifecycle metrics.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct JobReport {
    /// Items the run examined.
    pub processed: u64,
    /// Items the run changed.
    pub changed: u64,
}

/// What a running job sees: the serving state to work over, and a cooperative cancellation signal.
pub struct JobContext {
    state: Arc<ServingState>,
    cancel: tokio_util::sync::CancellationToken,
}

impl JobContext {
    /// The serving state: stores, caches, and configured indexes the job acts on.
    #[must_use]
    pub const fn state(&self) -> &Arc<ServingState> {
        &self.state
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

/// A unit of node-local maintenance the [`JobScheduler`] can run.
#[async_trait]
pub trait NodeJob: Send + Sync {
    /// A stable, low-cardinality label for this job's kind, used for metrics and conflict keys.
    fn kind(&self) -> &'static str;

    /// The repository or resource this run acts on. Two runs sharing a kind and scope conflict and
    /// never overlap; different scopes run concurrently. Empty names a node-wide task.
    fn scope(&self) -> &str;

    /// The durable job kind to record a run under, or `None` to run without a persisted history entry.
    fn persist_as(&self) -> Option<JobKind> {
        None
    }

    /// Do the work. A cooperative job polls `ctx` for cancellation and returns early when asked.
    ///
    /// # Errors
    /// Returns a user-visible message when the work fails.
    async fn run(&self, ctx: &JobContext) -> Result<JobReport, String>;
}

/// The server's maintenance pass for one ecosystem: reclaim expired process-local resources, then
/// revalidate that ecosystem's stale cached pages. Reclaim runs first so an upstream stall during the
/// refresh cannot extend an idle resource's deadline.
struct MaintenanceJob {
    driver: Arc<dyn EcosystemDriver>,
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

    async fn run(&self, ctx: &JobContext) -> Result<JobReport, String> {
        let ecosystem = self.driver.ecosystem();
        let reclaimed = self.driver.reclaim_idle(ctx.state().clone()).await;
        if reclaimed > 0 {
            tracing::info!(ecosystem = %ecosystem, reclaimed, "idle resources reclaimed");
        }
        if ctx.is_cancelled() {
            return Ok(JobReport::default());
        }
        let sweep = self.driver.refresh_stale(ctx.state().clone()).await?;
        if sweep.checked > 0 {
            tracing::info!(ecosystem = %ecosystem, ?sweep, "background refresh sweep");
        }
        Ok(JobReport {
            processed: sweep.checked as u64,
            changed: sweep.changed as u64,
        })
    }
}

/// Submit one maintenance job per installed ecosystem driver. The scheduler runs them concurrently
/// across ecosystems under its bounds and drops any whose predecessor is still sweeping.
pub fn submit_maintenance(app: &AppState, scheduler: &JobScheduler) {
    for driver in app.drivers() {
        scheduler.submit(Arc::new(MaintenanceJob { driver: driver.clone() }));
    }
}
