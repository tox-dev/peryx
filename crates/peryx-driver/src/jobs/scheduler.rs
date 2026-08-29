//! A submission is admitted only when a queue slot is free and no run with the same conflict key is
//! already in flight, so two conflicting repository jobs never overlap while independent repositories
//! run together. Admitted work spawns onto the Tokio runtime and acquires a global permit (the worker
//! bound), then a per-kind and a per-repository permit, before it runs. Shutdown signals cooperative
//! cancellation and then waits out a grace period before it returns.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use futures_util::FutureExt as _;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast, oneshot};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use peryx_storage::meta::{JobOutcome, MetaError, NewJobRun};

use super::attempts::{CancelJobRun, JobAttemptError};
use super::metrics::{JobMetrics, Outcome, Reject};
use super::{JobCompletion, JobContext, JobFailure, JobReport, JobRunOutcome, LeaseScope, NodeJob};
use crate::state::ServingState;

/// How long a cluster-singleton lease stays held before it lapses absent a renewal. A run claims at
/// start and releases at end, so the deadline only flags a holder that crashed mid-run; it is generous
/// enough to cover a bounded pass without a renewal loop.
const JOB_LEASE_SECS: i64 = 300;

/// This process's stable lease-holder identity, minted once and reused so a run's release matches its
/// claim. A restart is a new process and a new holder, which the ownership term supersedes.
fn node_holder() -> &'static str {
    static HOLDER: OnceLock<String> = OnceLock::new();
    HOLDER.get_or_init(|| format!("node-{}", std::process::id()))
}

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
    inflight: Mutex<Inflight>,
    metrics: Arc<JobMetrics>,
    completions: broadcast::Sender<JobCompletion>,
    cancel: CancellationToken,
}

/// The conflict keys of currently admitted runs, split by kind so a probe borrows the scope instead
/// of building a combined key: an admitted run stores one owned scope, and every refused submission
/// checks membership without allocating.
type Inflight = HashMap<&'static str, HashSet<Box<str>>>;

impl Shared {
    fn lock_inflight(&self) -> MutexGuard<'_, Inflight> {
        self.inflight.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Release the `(kind, scope)` conflict key held by a finished run, dropping the kind's set once it
    /// empties so the map never accumulates idle kinds.
    fn release_conflict(inflight: &mut Inflight, kind: &'static str, scope: &str) {
        let scopes = inflight.get_mut(kind).expect("an admitted run holds its conflict key");
        scopes.remove(scope);
        if scopes.is_empty() {
            inflight.remove(kind);
        }
    }
}

/// A node-local job scheduler: submit typed jobs, and it runs them under the configured bounds.
pub struct JobScheduler {
    shared: Arc<Shared>,
    tracker: TaskTracker,
    grace: Duration,
}

impl JobScheduler {
    #[must_use]
    pub fn new(state: Arc<ServingState>, limits: JobLimits) -> Self {
        let (completions, _) = broadcast::channel(limits.queue.get());
        let shared = Shared {
            state,
            workers: Arc::new(Semaphore::new(limits.workers.get())),
            queue: Arc::new(Semaphore::new(limits.queue.get())),
            per_kind: KeyedLimiter::new(limits.per_kind),
            per_repository: KeyedLimiter::new(limits.per_repository),
            inflight: Mutex::new(Inflight::new()),
            metrics: Arc::new(JobMetrics::default()),
            completions,
            cancel: CancellationToken::new(),
        };
        Self {
            shared: Arc::new(shared),
            tracker: TaskTracker::new(),
            grace: limits.shutdown_grace,
        }
    }

    #[must_use]
    pub fn metrics(&self) -> Arc<JobMetrics> {
        self.shared.metrics.clone()
    }

    /// Subscribe to completions published after this call.
    ///
    /// A receiver that falls behind the admitted-job bound gets [`broadcast::error::RecvError::Lagged`].
    #[must_use]
    pub fn subscribe_completions(&self) -> broadcast::Receiver<JobCompletion> {
        self.shared.completions.subscribe()
    }

    /// Refusal is a normal outcome, not an error: a duplicate is a [`Conflict`](Submit::Conflict), a
    /// saturated queue is [`QueueFull`](Submit::QueueFull), and a draining scheduler is
    /// [`ShuttingDown`](Submit::ShuttingDown). Only an admitted job spawns.
    pub fn submit(&self, job: Arc<dyn NodeJob>) -> Submit {
        self.admit(job, None)
    }

    /// # Panics
    /// Panics if an admitted worker exits without sending its result, which violates the scheduler's
    /// completion invariant.
    ///
    /// # Errors
    /// Returns a user-visible message when admission is refused or execution fails.
    pub async fn run(&self, job: Arc<dyn NodeJob>) -> Result<JobRunOutcome, String> {
        let (sender, receiver) = oneshot::channel();
        match self.admit(job, Some(sender)) {
            Submit::Queued => receiver.await.expect("an admitted job sends its result"),
            Submit::Conflict => Err("a matching node-local job is already running".to_owned()),
            Submit::QueueFull => Err("the node-local job queue is full".to_owned()),
            Submit::ShuttingDown => Err("the node-local job scheduler is shutting down".to_owned()),
        }
    }

    /// Signal one running durable attempt without affecting other work.
    ///
    /// # Errors
    /// Returns a store error if the ID is not active and its durable record cannot be inspected.
    pub fn cancel_job_run(&self, id: &str) -> Result<CancelJobRun, MetaError> {
        self.shared.state.job_attempts.cancel(id)
    }

    fn admit(
        &self,
        job: Arc<dyn NodeJob>,
        completion: Option<oneshot::Sender<Result<JobRunOutcome, String>>>,
    ) -> Submit {
        let kind = job.kind();
        if self.shared.cancel.is_cancelled() {
            return Submit::ShuttingDown;
        }
        let scope = job.scope();
        let mut inflight = self.shared.lock_inflight();
        if inflight.get(kind).is_some_and(|scopes| scopes.contains(scope)) {
            drop(inflight);
            self.shared.metrics.rejected(kind, Reject::Conflict);
            return Submit::Conflict;
        }
        let Ok(slot) = self.shared.queue.clone().try_acquire_owned() else {
            drop(inflight);
            self.shared.metrics.rejected(kind, Reject::QueueFull);
            return Submit::QueueFull;
        };
        inflight.entry(kind).or_default().insert(Box::from(scope));
        drop(inflight);
        self.tracker
            .spawn(run_admitted(self.shared.clone(), job, slot, completion));
        Submit::Queued
    }

    pub async fn shutdown(&self) {
        self.shared.cancel.cancel();
        self.tracker.close();
        if timeout(self.grace, self.tracker.wait()).await.is_err() {
            tracing::warn!("node-local jobs did not finish within the shutdown grace period");
        }
    }
}

async fn run_admitted(
    shared: Arc<Shared>,
    job: Arc<dyn NodeJob>,
    slot: OwnedSemaphorePermit,
    completion: Option<oneshot::Sender<Result<JobRunOutcome, String>>>,
) {
    let _slot = slot;
    let _worker = shared
        .workers
        .clone()
        .acquire_owned()
        .await
        .expect("worker semaphore stays open");
    let _kind = shared.per_kind.acquire(job.kind()).await;
    let _repository = shared.per_repository.acquire(job.scope()).await;
    let result = execute(job.as_ref(), &shared, &shared.cancel.child_token()).await;
    Shared::release_conflict(&mut shared.lock_inflight(), job.kind(), job.scope());
    if let Some(completion) = completion {
        let _ = completion.send(result.map_err(|error| error.to_string()));
    }
}

async fn execute(job: &dyn NodeJob, shared: &Shared, cancel: &CancellationToken) -> Result<JobRunOutcome, JobError> {
    let kind = job.kind();
    shared.metrics.started(kind);
    let (outcome, result) = if cancel.is_cancelled() {
        (Outcome::Cancelled, Ok(JobRunOutcome::cancelled(JobReport::default())))
    } else {
        let (result, outcome) = run_persisted(job, shared, cancel).await;
        let scope = job.scope();
        match &result {
            Ok(JobRunOutcome::Succeeded(report)) => {
                tracing::info!(kind, scope, ?report, "node-local job finished");
            }
            Ok(JobRunOutcome::Cancelled(_)) => {}
            Err(error) => tracing::error!(kind, scope, %error, "node-local job failed"),
        }
        (outcome, result)
    };
    shared.metrics.finished(kind, outcome);
    let _ = shared.completions.send(JobCompletion::new(
        kind,
        outcome,
        result.as_ref().ok().copied().map(JobRunOutcome::report),
    ));
    result
}

async fn run_persisted(
    job: &dyn NodeJob,
    shared: &Shared,
    cancel: &CancellationToken,
) -> (Result<JobRunOutcome, JobError>, Outcome) {
    let run = match job.persist_as() {
        Some(kind) => match shared.state.job_attempts.start(
            NewJobRun {
                kind,
                scope: job.scope(),
                repository: job.repository(),
                started_at_unix: (shared.state.clock)(),
            },
            cancel.clone(),
        ) {
            Ok(id) => Some(id),
            Err(error) => return (Err(error.into()), Outcome::Failed),
        },
        None => None,
    };
    // Take the run's fence: a per-repository job leases its authority epoch, a cluster-singleton job
    // claims a control-plane lease under the ownership term, and either can be fenced before it runs when
    // a newer holder already owns the work.
    let (result, panicked) = match acquire_fence(job, shared).await {
        Acquired::Held(fence) => {
            let context = JobContext {
                state: shared.state.clone(),
                cancel: cancel.clone(),
                fence: fence.epoch(),
            };
            let (mut result, panicked) = AssertUnwindSafe(job.run(&context)).catch_unwind().await.map_or_else(
                |_| (Err(JobFailure::new("job_panic", "node-local job panicked")), true),
                |result| (result, false),
            );
            // A run whose authority or lease was superseded while it ran wrote under a stale fence, so its
            // success is rejected rather than counted; the lease is released either way.
            if let Some(fenced) = finish_fence(&fence, shared, !panicked && result.is_ok()).await {
                result = Err(fenced);
            }
            (result, panicked)
        }
        Acquired::NotAcquired(reason) => (Err(JobFailure::new("lease_not_held", reason)), false),
    };
    let outcome = if panicked {
        Outcome::Failed
    } else {
        match &result {
            Ok(JobRunOutcome::Succeeded(_)) => Outcome::Succeeded,
            Ok(JobRunOutcome::Cancelled(_)) => Outcome::Cancelled,
            Err(_) => Outcome::Failed,
        }
    };
    if let Some(id) = run {
        let finished_at_unix = (shared.state.clock)();
        let error = result.as_ref().err().map(ToString::to_string);
        let persisted = if let Ok(JobRunOutcome::Cancelled(report)) = &result {
            JobOutcome::cancelled(finished_at_unix, report.processed, report.changed)
                .with_quota(report.quota_released, report.quota_remaining)
        } else if let Ok(JobRunOutcome::Succeeded(report)) = &result {
            JobOutcome::succeeded(finished_at_unix, report.processed, report.changed)
                .with_quota(report.quota_released, report.quota_remaining)
        } else {
            JobOutcome::failed(
                finished_at_unix,
                0,
                0,
                error.as_deref().expect("failed job carries an error"),
            )
        };
        if let Err(error) = shared.state.job_attempts.finish(&id, persisted) {
            return (Err(error.into()), Outcome::Failed);
        }
    }
    (result.map_err(JobError::from), outcome)
}

/// The fence a run holds while it executes, and the ownership it re-checks and releases afterward.
enum RunFence {
    /// A per-repository job fenced by its authority epoch, or a node-local job with no repository at the
    /// closed `0` sentinel, which is never fenced.
    Repository { repository: Option<String>, epoch: u64 },
    /// A cluster-singleton job holding a control-plane lease on `key` under the ownership term `epoch`.
    Singleton { key: String, epoch: u64 },
}

impl RunFence {
    /// The epoch a run stamps onto the records it writes, so a later holder fences it out.
    const fn epoch(&self) -> u64 {
        match self {
            Self::Repository { epoch, .. } | Self::Singleton { epoch, .. } => *epoch,
        }
    }
}

/// The outcome of taking a run's fence before it executes.
enum Acquired {
    /// The fence is held; the run may execute.
    Held(RunFence),
    /// The cluster-singleton lease could not be claimed - a newer holder owns the term, or the store
    /// could not be reached - so the run does not start and a later tick re-drives it. Carries the
    /// reason for the durable run record.
    NotAcquired(String),
}

/// Take the fence a run executes under: a node-local job leases its repository authority epoch without a
/// control-plane call, and a cluster-singleton job claims a durable lease under the ownership term.
async fn acquire_fence(job: &dyn NodeJob, shared: &Shared) -> Acquired {
    match job.lease_scope() {
        LeaseScope::NodeLocal => {
            let epoch = match job.repository() {
                Some(repository) => shared.state.committed_authority_epoch(repository).await,
                None => 0,
            };
            Acquired::Held(RunFence::Repository {
                repository: job.repository().map(ToOwned::to_owned),
                epoch,
            })
        }
        LeaseScope::ClusterSingleton(key) => {
            let epoch = shared.state.cluster_term();
            let now = (shared.state.clock)();
            match shared
                .state
                .meta
                .claim_job_lease(&key, node_holder(), epoch, now, JOB_LEASE_SECS)
            {
                Ok(_) => Acquired::Held(RunFence::Singleton { key, epoch }),
                Err(error) => Acquired::NotAcquired(error.to_string()),
            }
        }
    }
}

/// Re-check a finished run's fence and release a singleton lease. Returns the fencing failure when a
/// newer holder superseded a run that would otherwise have succeeded; `ok` is whether the run produced a
/// success to fence.
async fn finish_fence(fence: &RunFence, shared: &Shared, ok: bool) -> Option<JobFailure> {
    match fence {
        RunFence::Repository {
            repository: Some(repository),
            epoch,
        } if *epoch != 0 => (ok && !shared.state.admit_authority_epoch(repository, *epoch).await)
            .then(|| JobFailure::new("authority_fenced", "a newer authority epoch superseded this run")),
        RunFence::Repository { .. } => None,
        RunFence::Singleton { key, epoch } => {
            let superseded = shared
                .state
                .meta
                .job_lease(key)
                .ok()
                .flatten()
                .is_some_and(|lease| lease.epoch > *epoch);
            // Release at our epoch; a release the supersede already fenced is a harmless stale no-op.
            let _ = shared.state.meta.release_job_lease(key, node_holder(), *epoch);
            (ok && superseded).then(|| {
                JobFailure::new(
                    "authority_fenced",
                    "a newer holder superseded this cluster-singleton run",
                )
            })
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum JobError {
    #[error("{0}")]
    Job(#[from] JobFailure),
    #[error(transparent)]
    Attempt(#[from] JobAttemptError),
}
