//! One bounded timer drives every schedule from a single min-heap keyed by next-due instant, so a
//! large schedule set costs one heap pop per fire rather than a scan of every entry on each tick. The
//! timer keeps no durable state: a restart recomputes each schedule's next run one interval after
//! startup and never replays the occurrences missed while the process was down.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use peryx_core::Ecosystem;

use super::{
    CACHE_MAINTENANCE, JobHistoryCleanup, JobScheduler, MAINTENANCE_INTERVAL, WriteLedgerReap, scheduled_job,
    submit_maintenance,
};
use crate::state::AppState;

pub trait ScheduledJobFactory: Send + Sync {
    fn kind(&self) -> &'static str;
    fn settings(&self) -> toml::Table;

    /// # Errors
    /// Returns a user-facing configuration error when runtime state cannot satisfy the compiled job.
    fn create(&self, app: &AppState) -> Result<Arc<dyn super::NodeJob>, String>;
}

#[derive(Clone)]
pub struct PluginScheduledJob {
    ecosystem: Ecosystem,
    pub(super) factory: Arc<dyn ScheduledJobFactory>,
}

impl PluginScheduledJob {
    #[must_use]
    pub fn new(ecosystem: Ecosystem, factory: Arc<dyn ScheduledJobFactory>) -> Self {
        Self { ecosystem, factory }
    }

    #[must_use]
    pub fn ecosystem(&self) -> Ecosystem {
        self.ecosystem.clone()
    }

    #[must_use]
    pub fn kind(&self) -> &'static str {
        self.factory.kind()
    }

    #[must_use]
    pub fn settings(&self) -> toml::Table {
        self.factory.settings()
    }
}

impl std::fmt::Debug for PluginScheduledJob {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PluginScheduledJob")
            .field("ecosystem", &self.ecosystem)
            .field("kind", &self.kind())
            .field("settings", &self.settings())
            .finish_non_exhaustive()
    }
}

impl PartialEq for PluginScheduledJob {
    fn eq(&self, other: &Self) -> bool {
        self.ecosystem == other.ecosystem && self.kind() == other.kind() && self.settings() == other.settings()
    }
}

impl Eq for PluginScheduledJob {}

#[derive(Clone)]
pub struct RegisteredScheduledJob {
    pub(super) factory: Arc<dyn ScheduledJobFactory>,
}

impl RegisteredScheduledJob {
    #[must_use]
    pub fn new(factory: Arc<dyn ScheduledJobFactory>) -> Self {
        Self { factory }
    }

    #[must_use]
    pub fn kind(&self) -> &'static str {
        self.factory.kind()
    }

    #[must_use]
    pub fn settings(&self) -> toml::Table {
        self.factory.settings()
    }
}

impl std::fmt::Debug for RegisteredScheduledJob {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RegisteredScheduledJob")
            .field("kind", &self.kind())
            .field("settings", &self.settings())
            .finish_non_exhaustive()
    }
}

impl PartialEq for RegisteredScheduledJob {
    fn eq(&self, other: &Self) -> bool {
        self.kind() == other.kind() && self.settings() == other.settings()
    }
}

impl Eq for RegisteredScheduledJob {}

/// A registered node-local job kind a schedule can name.
///
/// Each kind expands into the concrete [`NodeJob`](super::NodeJob)s to run when it fires: cache
/// maintenance fans out one per installed ecosystem driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduledJob {
    /// Reclaim idle process resources and revalidate stale cached pages, per ecosystem.
    CacheMaintenance,
    Plugin(PluginScheduledJob),
    Registered(RegisteredScheduledJob),
}

impl ScheduledJob {
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::CacheMaintenance => CACHE_MAINTENANCE,
            Self::Plugin(job) => job.kind(),
            Self::Registered(job) => job.kind(),
        }
    }

    #[must_use]
    pub fn settings(&self) -> toml::Table {
        match self {
            Self::CacheMaintenance => toml::Table::new(),
            Self::Plugin(job) => job.settings(),
            Self::Registered(job) => job.settings(),
        }
    }

    fn submit(&self, app: &AppState, scheduler: &JobScheduler) {
        match self {
            Self::CacheMaintenance => submit_maintenance(app, scheduler),
            Self::Plugin(_) | Self::Registered(_) => match scheduled_job(app, self) {
                Ok(job) => {
                    scheduler.submit(job);
                }
                Err(error) => {
                    let job = self.as_str();
                    tracing::error!(job, %error, "scheduled job rejected");
                }
            },
        }
    }
}

/// One configured schedule: a job kind and the interval between its runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    pub job: ScheduledJob,
    pub interval: Duration,
}

/// Run each schedule on its interval until `cancel` fires, submitting due jobs through `scheduler`.
///
/// An internal history cleanup runs immediately and once per maintenance interval even with no
/// configured schedule. Each fire reschedules one interval on. A fire that wakes past its due instant,
/// from a slow tick or a clock advanced across several intervals, reschedules from the wake instant,
/// so missed occurrences collapse into the next run rather than replaying as a backlog. When the
/// scheduler refuses a submission because the same job is still running, the timer counts that
/// skipped tick in the scheduler's metrics and moves on to the next fire.
pub async fn run_schedules(
    app: Arc<AppState>,
    scheduler: Arc<JobScheduler>,
    plan: Vec<Schedule>,
    cancel: CancellationToken,
) {
    ScheduleTimer::new(plan).run(&app, &scheduler, cancel).await;
}

pub(super) struct ScheduleTimer {
    plan: Vec<Schedule>,
    cleanup: usize,
    due: BinaryHeap<Reverse<(Instant, usize)>>,
}

impl ScheduleTimer {
    pub(super) fn new(plan: Vec<Schedule>) -> Self {
        let start = Instant::now();
        let cleanup = plan.len();
        let due = plan
            .iter()
            .enumerate()
            .map(|(index, schedule)| Reverse((start + schedule.interval, index)))
            .chain(std::iter::once(Reverse((start, cleanup))))
            .collect();
        Self { plan, cleanup, due }
    }

    pub(super) async fn run(mut self, app: &AppState, scheduler: &JobScheduler, cancel: CancellationToken) {
        while let Some(Reverse((at, index))) = self.due.pop() {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep_until(at) => {}
            }
            let interval = if index == self.cleanup {
                scheduler.submit(Arc::new(JobHistoryCleanup::default()));
                scheduler.submit(Arc::new(WriteLedgerReap::default()));
                MAINTENANCE_INTERVAL
            } else {
                let schedule = &self.plan[index];
                let job = schedule.job.as_str();
                tracing::debug!(job, "schedule fired");
                schedule.job.submit(app, scheduler);
                schedule.interval
            };
            self.due
                .push(Reverse((reschedule(at, Instant::now(), interval), index)));
        }
    }
}

/// One interval past the due instant holds a steady cadence. When the fire woke past its due instant,
/// from a slow tick or a clock advanced across intervals, the next run is one interval past the wake
/// instant instead, so a long gap yields a single run rather than a replayed backlog.
pub(super) fn reschedule(at: Instant, woke: Instant, interval: Duration) -> Instant {
    let next = at + interval;
    if next <= woke { woke + interval } else { next }
}
