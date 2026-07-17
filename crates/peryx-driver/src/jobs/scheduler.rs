//! The bounded worker pool that runs [`NodeJob`]s under global, per-kind, and per-repository limits.
//!
//! A submission is admitted only when a queue slot is free and no run with the same conflict key is
//! already in flight, so two conflicting repository jobs never overlap while independent repositories
//! run together. Admitted work spawns onto the Tokio runtime and acquires a global permit (the worker
//! bound), then a per-kind and a per-repository permit, before it runs. Shutdown signals cooperative
//! cancellation and then waits out a grace period before it returns.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use peryx_storage::meta::{JobOutcome, JobState, MetaError, NewJobRun};

use super::metrics::{JobMetrics, Outcome, Reject};
use super::{JobContext, JobReport, NodeJob};
use crate::state::ServingState;

/// The bounds a [`JobScheduler`] runs under.
#[derive(Debug, Clone, Copy)]
pub struct JobLimits {
    /// Jobs allowed to run at once across every kind and repository.
    pub workers: NonZeroUsize,
    /// Admitted-but-unfinished jobs allowed to wait; a submission past this is rejected.
    pub queue: NonZeroUsize,
    /// Jobs of one kind allowed to run at once.
    pub per_kind: NonZeroUsize,
    /// Jobs acting on one repository allowed to run at once.
    pub per_repository: NonZeroUsize,
    /// How long [`shutdown`](JobScheduler::shutdown) waits for cancelled work before returning.
    pub shutdown_grace: Duration,
}

impl JobLimits {
    /// The defaults for a single node's maintenance: a handful of workers, a deep queue that absorbs a
    /// full sweep's fan-out, one run per repository so a repository never sweeps itself twice at once,
    /// and a shutdown grace that lets an in-flight sweep unwind.
    #[must_use]
    pub const fn node_local() -> Self {
        const fn nz(value: usize) -> NonZeroUsize {
            NonZeroUsize::new(value).expect("literal is non-zero")
        }
        Self {
            workers: nz(4),
            queue: nz(128),
            per_kind: nz(4),
            per_repository: nz(1),
            shutdown_grace: Duration::from_secs(30),
        }
    }
}

/// What became of a [`submit`](JobScheduler::submit) call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submit {
    /// Admitted; the job is queued or running.
    Queued,
    /// A run with the same kind and scope is already in flight, so this one was dropped.
    Conflict,
    /// The queue is full; the job was dropped rather than made to wait unbounded.
    QueueFull,
    /// The scheduler is shutting down and accepts no new work.
    ShuttingDown,
}

/// A set of permits keyed by an arbitrary string, each with the same capacity.
///
/// The node-local scheduler keys these by job kind and by ecosystem, both bounded sets, so the map
/// stays small; it is not sized for an unbounded key space.
struct KeyedLimiter {
    capacity: usize,
    permits: Mutex<HashMap<String, Arc<Semaphore>>>,
}

impl KeyedLimiter {
    fn new(capacity: NonZeroUsize) -> Self {
        Self {
            capacity: capacity.get(),
            permits: Mutex::new(HashMap::new()),
        }
    }

    async fn acquire(&self, key: &str) -> OwnedSemaphorePermit {
        let semaphore = {
            let mut permits = self.permits.lock().unwrap_or_else(PoisonError::into_inner);
            permits
                .entry(key.to_owned())
                .or_insert_with(|| Arc::new(Semaphore::new(self.capacity)))
                .clone()
        };
        semaphore.acquire_owned().await.expect("keyed semaphore stays open")
    }
}

/// The state a scheduler shares with each admitted job it spawns.
struct Shared {
    state: Arc<ServingState>,
    workers: Arc<Semaphore>,
    queue: Arc<Semaphore>,
    per_kind: KeyedLimiter,
    per_repository: KeyedLimiter,
    inflight: Mutex<HashSet<String>>,
    metrics: Arc<JobMetrics>,
    cancel: CancellationToken,
}

impl Shared {
    fn lock_inflight(&self) -> MutexGuard<'_, HashSet<String>> {
        self.inflight.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A node-local job scheduler: submit typed jobs, and it runs them under the configured bounds.
pub struct JobScheduler {
    shared: Arc<Shared>,
    tracker: TaskTracker,
    grace: Duration,
}

impl JobScheduler {
    /// Build a scheduler over `state`'s stores and clock with the given `limits`.
    #[must_use]
    pub fn new(state: Arc<ServingState>, limits: JobLimits) -> Self {
        let shared = Shared {
            state,
            workers: Arc::new(Semaphore::new(limits.workers.get())),
            queue: Arc::new(Semaphore::new(limits.queue.get())),
            per_kind: KeyedLimiter::new(limits.per_kind),
            per_repository: KeyedLimiter::new(limits.per_repository),
            inflight: Mutex::new(HashSet::new()),
            metrics: Arc::new(JobMetrics::default()),
            cancel: CancellationToken::new(),
        };
        Self {
            shared: Arc::new(shared),
            tracker: TaskTracker::new(),
            grace: limits.shutdown_grace,
        }
    }

    /// The lifecycle counters, to register as a process metric source.
    #[must_use]
    pub fn metrics(&self) -> Arc<JobMetrics> {
        self.shared.metrics.clone()
    }

    /// Admit `job` for execution, or report why it was refused.
    ///
    /// Refusal is a normal outcome, not an error: a duplicate is a [`Conflict`](Submit::Conflict), a
    /// saturated queue is [`QueueFull`](Submit::QueueFull), and a draining scheduler is
    /// [`ShuttingDown`](Submit::ShuttingDown). Only an admitted job spawns.
    pub fn submit(&self, job: Arc<dyn NodeJob>) -> Submit {
        let kind = job.kind();
        if self.shared.cancel.is_cancelled() {
            return Submit::ShuttingDown;
        }
        let key = conflict_key(kind, job.scope());
        if !self.shared.lock_inflight().insert(key.clone()) {
            self.shared.metrics.rejected(kind, Reject::Conflict);
            return Submit::Conflict;
        }
        let Ok(slot) = self.shared.queue.clone().try_acquire_owned() else {
            self.shared.lock_inflight().remove(&key);
            self.shared.metrics.rejected(kind, Reject::QueueFull);
            return Submit::QueueFull;
        };
        self.tracker.spawn(run_admitted(self.shared.clone(), job, key, slot));
        Submit::Queued
    }

    /// Stop accepting work, signal cooperative cancellation, and wait for running jobs to unwind,
    /// returning once they finish or the grace period elapses, whichever comes first.
    pub async fn shutdown(&self) {
        self.shared.cancel.cancel();
        self.tracker.close();
        if timeout(self.grace, self.tracker.wait()).await.is_err() {
            tracing::warn!("node-local jobs did not finish within the shutdown grace period");
        }
    }
}

async fn run_admitted(shared: Arc<Shared>, job: Arc<dyn NodeJob>, key: String, slot: OwnedSemaphorePermit) {
    let _slot = slot;
    let _worker = shared
        .workers
        .clone()
        .acquire_owned()
        .await
        .expect("worker semaphore stays open");
    let _kind = shared.per_kind.acquire(job.kind()).await;
    let _repository = shared.per_repository.acquire(job.scope()).await;
    execute(job.as_ref(), &shared.state, &shared.cancel, &shared.metrics).await;
    shared.lock_inflight().remove(&key);
}

fn conflict_key(kind: &str, scope: &str) -> String {
    format!("{kind}\u{0}{scope}")
}

async fn execute(job: &dyn NodeJob, state: &Arc<ServingState>, cancel: &CancellationToken, metrics: &JobMetrics) {
    let kind = job.kind();
    metrics.started(kind);
    let outcome = if cancel.is_cancelled() {
        Outcome::Cancelled
    } else {
        match run_persisted(job, state, cancel).await {
            Ok(_) if cancel.is_cancelled() => Outcome::Cancelled,
            Ok(report) => {
                tracing::info!(kind, scope = job.scope(), ?report, "node-local job finished");
                Outcome::Succeeded
            }
            Err(error) => {
                tracing::error!(kind, scope = job.scope(), %error, "node-local job failed");
                Outcome::Failed
            }
        }
    };
    metrics.finished(kind, outcome);
}

async fn run_persisted(
    job: &dyn NodeJob,
    state: &Arc<ServingState>,
    cancel: &CancellationToken,
) -> Result<JobReport, JobError> {
    let run = match job.persist_as() {
        Some(kind) => Some(state.meta.start_job_run(NewJobRun {
            kind,
            scope: job.scope(),
            started_at_unix: (state.clock)(),
        })?),
        None => None,
    };
    let context = JobContext {
        state: state.clone(),
        cancel: cancel.clone(),
    };
    let result = job.run(&context).await;
    if let Some(id) = run {
        let outcome = match &result {
            Ok(report) => JobOutcome {
                state: JobState::Succeeded,
                finished_at_unix: (state.clock)(),
                items_processed: report.processed,
                items_changed: report.changed,
                error: None,
            },
            Err(message) => JobOutcome {
                state: JobState::Failed,
                finished_at_unix: (state.clock)(),
                items_processed: 0,
                items_changed: 0,
                error: Some(message.as_str()),
            },
        };
        state.meta.finish_job_run(&id, outcome)?;
    }
    Ok(result?)
}

#[derive(Debug, thiserror::Error)]
enum JobError {
    #[error("{0}")]
    Job(String),
    #[error(transparent)]
    Store(#[from] MetaError),
}

impl From<String> for JobError {
    fn from(message: String) -> Self {
        Self::Job(message)
    }
}
