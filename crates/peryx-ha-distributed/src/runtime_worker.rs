//! A bounded runtime for background availability work, held off the foreground request executor.
//!
//! `dc` and `ha` replicas apply the writer's changelog on a loop and copy blobs, work that must not
//! share the Tokio workers serving package traffic nor grow an unbounded blocking pool. A [replica]
//! process therefore drives that work on a dedicated [`AvailabilityRuntime`] with a fixed worker
//! count, a bounded blocking pool, and a bounded set of concurrency slots: a full set returns
//! backpressure to the caller instead of overcommitting the runtime. A `none` process constructs
//! none of this.
//!
//! [replica]: @/core/high-availability.md

use std::fmt::Write as _;
use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread::available_parallelism;

use anyhow::Context as _;
use peryx_driver::PrometheusSource;
use tokio::runtime::{Builder, Handle, Runtime};

use crate::{AnalyticsPuller, BeaconSender, ReplicaLoop};

/// The most worker threads the availability runtime starts, whatever the host reports. A container
/// that sees many host cores must not spawn one background worker per core; replication is a
/// throughput-bounded trickle beside foreground request serving.
const WORKER_THREAD_CAP: usize = 4;

/// The most blocking threads the availability runtime keeps for filesystem and checksum work, so a
/// backlog of blob copies never opens an unbounded pool nor starves the foreground executor.
const BLOCKING_THREAD_CAP: usize = 8;

/// The number of concurrent background tasks the runtime admits. One resident replica loop holds a
/// slot for the process lifetime; the remainder bound the copy and apply work later issues submit.
const WORKER_SLOTS: usize = 32;

/// A background task submitted to the availability runtime, type-erased so the runtime carries one
/// supervisor rather than a monomorphization per future.
type BackgroundTask = Pin<Box<dyn Future<Output = ()> + Send>>;

/// The worker thread count for this host: the reported parallelism, clamped to [`WORKER_THREAD_CAP`].
/// [`available_parallelism`] honors the process CPU affinity, so a pinned container starts fewer
/// threads rather than assuming the machine's full core count.
fn worker_thread_count() -> usize {
    available_parallelism()
        .map_or(1, NonZeroUsize::get)
        .min(WORKER_THREAD_CAP)
}

/// Shared concurrency accounting for availability workers.
#[derive(Debug)]
pub struct WorkerShared {
    worker_threads: usize,
    total_slots: usize,
    in_flight: AtomicUsize,
    rejected: AtomicU64,
    panics: AtomicU64,
    healthy: AtomicBool,
}

impl WorkerShared {
    const fn new(worker_threads: usize, total_slots: usize) -> Self {
        Self {
            worker_threads,
            total_slots,
            in_flight: AtomicUsize::new(0),
            rejected: AtomicU64::new(0),
            panics: AtomicU64::new(0),
            healthy: AtomicBool::new(true),
        }
    }

    /// The shared state a replica process starts with: a runtime-sized worker pool and the default
    /// slot budget.
    #[must_use]
    pub fn for_replica() -> Self {
        Self::new(worker_thread_count(), WORKER_SLOTS)
    }

    /// Whether the worker domain still serves. A task panic clears this, so a replica's readiness
    /// can report the domain unhealthy while the process keeps serving reads under its staleness
    /// contract.
    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    /// Take one concurrency slot, or `None` when every slot is in use so the caller feels
    /// backpressure. A rejected reservation is counted rather than dropped silently.
    fn reserve(self: &Arc<Self>) -> Option<SlotGuard> {
        if self.in_flight.fetch_add(1, Ordering::AcqRel) >= self.total_slots {
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            self.rejected.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        Some(SlotGuard {
            shared: Arc::clone(self),
        })
    }

    pub fn record_panic(&self) {
        self.panics.fetch_add(1, Ordering::Relaxed);
        self.healthy.store(false, Ordering::Relaxed);
    }
}

/// A held concurrency slot, released to the runtime when the task that took it ends.
struct SlotGuard {
    shared: Arc<WorkerShared>,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.shared.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

impl PrometheusSource for WorkerShared {
    fn write_metrics(&self, body: &mut String) {
        let _ = write!(
            body,
            "# HELP peryx_availability_worker_threads Worker threads the availability runtime runs.\n\
             # TYPE peryx_availability_worker_threads gauge\n\
             peryx_availability_worker_threads {}\n\
             # HELP peryx_availability_worker_slots Concurrent background tasks the runtime admits.\n\
             # TYPE peryx_availability_worker_slots gauge\n\
             peryx_availability_worker_slots {}\n\
             # HELP peryx_availability_worker_slots_active Background tasks currently holding a slot.\n\
             # TYPE peryx_availability_worker_slots_active gauge\n\
             peryx_availability_worker_slots_active {}\n\
             # HELP peryx_availability_worker_rejected_total Task submissions refused for saturation.\n\
             # TYPE peryx_availability_worker_rejected_total counter\n\
             peryx_availability_worker_rejected_total {}\n\
             # HELP peryx_availability_worker_panics_total Background tasks that panicked.\n\
             # TYPE peryx_availability_worker_panics_total counter\n\
             peryx_availability_worker_panics_total {}\n",
            self.worker_threads,
            self.total_slots,
            self.in_flight.load(Ordering::Relaxed),
            self.rejected.load(Ordering::Relaxed),
            self.panics.load(Ordering::Relaxed),
        );
    }
}

/// A runtime that isolates availability work from request handling.
pub struct AvailabilityRuntime {
    runtime: Option<Runtime>,
    handle: Handle,
    shared: Arc<WorkerShared>,
}

impl AvailabilityRuntime {
    /// Build the runtime for a process that shares `shared` with its readiness surface and metrics.
    ///
    /// # Errors
    /// Returns the underlying error when the operating system refuses the worker threads.
    pub fn start(shared: Arc<WorkerShared>) -> std::io::Result<Self> {
        let runtime = Builder::new_multi_thread()
            .worker_threads(shared.worker_threads)
            .max_blocking_threads(BLOCKING_THREAD_CAP)
            .thread_name("peryx-availability")
            .enable_all()
            .build()?;
        let handle = runtime.handle().clone();
        Ok(Self {
            runtime: Some(runtime),
            handle,
            shared,
        })
    }

    /// Take a slot and run `task` on the runtime, returning its supervisor handle, or `None` when the
    /// runtime is saturated so the caller applies backpressure. A task that panics releases its slot
    /// and marks the worker domain unhealthy rather than aborting the process.
    pub fn try_spawn(&self, task: BackgroundTask) -> Option<tokio::task::JoinHandle<()>> {
        let guard = self.shared.reserve()?;
        let shared = Arc::clone(&self.shared);
        Some(self.handle.spawn(async move {
            let _guard = guard;
            // A cancelled task at shutdown also joins with an error; only an actual panic marks the
            // domain unhealthy.
            if tokio::spawn(task).await.is_err_and(|error| error.is_panic()) {
                shared.record_panic();
            }
        }))
    }

    /// Start the replica services on this runtime.
    ///
    /// # Errors
    /// Returns an error when the runtime cannot reserve a slot for a configured service.
    pub fn start_replica_services(
        self,
        replica: ReplicaLoop,
        analytics: Option<AnalyticsPuller>,
        beacon: Option<BeaconSender>,
    ) -> anyhow::Result<Self> {
        self.try_spawn(Box::pin(replica.run()))
            .context("reserve the replica loop slot")?;
        if let Some(analytics) = analytics {
            self.try_spawn(Box::pin(analytics.run()))
                .context("reserve the analytics pull slot")?;
        }
        if let Some(beacon) = beacon {
            self.try_spawn(Box::pin(beacon.run()))
                .context("reserve the frontier beacon slot")?;
        }
        Ok(self)
    }
}

impl Drop for AvailabilityRuntime {
    fn drop(&mut self) {
        // The owner drops the runtime from within the foreground runtime's context at shutdown, so
        // the teardown must not block; `shutdown_background` stops the workers and drops resident
        // tasks without waiting. A replica reapplies any uncommitted background page on restart.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(shared: &WorkerShared) -> String {
        let mut body = String::new();
        shared.write_metrics(&mut body);
        body
    }

    #[test]
    fn test_worker_thread_count_stays_within_the_cap() {
        let count = worker_thread_count();
        assert!((1..=WORKER_THREAD_CAP).contains(&count), "{count}");
    }

    #[test]
    fn test_reserve_hands_out_every_slot_then_applies_backpressure() {
        let shared = Arc::new(WorkerShared::new(2, 2));
        let first = shared.reserve().expect("first slot");
        let second = shared.reserve().expect("second slot");
        assert!(shared.reserve().is_none(), "third reservation must be refused");
        assert_eq!(shared.in_flight.load(Ordering::Relaxed), 2);
        assert_eq!(shared.rejected.load(Ordering::Relaxed), 1);
        drop(first);
        drop(second);
        assert_eq!(shared.in_flight.load(Ordering::Relaxed), 0);
        assert!(shared.reserve().is_some(), "a released slot is reusable");
    }

    #[test]
    fn test_metrics_report_slots_threads_and_counters() {
        let shared = Arc::new(WorkerShared::new(3, 4));
        let _held = shared.reserve().expect("slot");
        let body = rendered(&shared);
        assert!(body.contains("peryx_availability_worker_threads 3\n"), "{body}");
        assert!(body.contains("peryx_availability_worker_slots 4\n"), "{body}");
        assert!(body.contains("peryx_availability_worker_slots_active 1\n"), "{body}");
        assert!(body.contains("peryx_availability_worker_rejected_total 0\n"), "{body}");
        assert!(body.contains("peryx_availability_worker_panics_total 0\n"), "{body}");
    }

    #[test]
    fn test_runtime_runs_a_task_to_completion() {
        let shared = Arc::new(WorkerShared::for_replica());
        let runtime = AvailabilityRuntime::start(Arc::clone(&shared)).expect("runtime");
        let done = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&done);
        let handle = runtime
            .try_spawn(Box::pin(async move { flag.store(true, Ordering::Relaxed) }))
            .expect("slot");
        runtime.handle.block_on(handle).expect("task joins");
        assert!(done.load(Ordering::Relaxed));
        assert!(shared.is_healthy());
    }

    #[test]
    fn test_task_panic_marks_the_domain_unhealthy() {
        let shared = Arc::new(WorkerShared::new(1, 1));
        let runtime = AvailabilityRuntime::start(Arc::clone(&shared)).expect("runtime");
        let handle = runtime.try_spawn(Box::pin(async { panic!("boom") })).expect("slot");
        runtime.handle.block_on(handle).expect("supervisor joins");
        assert!(!shared.is_healthy());
        assert_eq!(shared.panics.load(Ordering::Relaxed), 1);
        assert!(rendered(&shared).contains("peryx_availability_worker_panics_total 1\n"));
    }

    #[test]
    fn test_shutdown_stops_resident_work() {
        struct SendOnDrop(std::sync::mpsc::Sender<()>);
        impl Drop for SendOnDrop {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }

        let shared = Arc::new(WorkerShared::for_replica());
        let runtime = AvailabilityRuntime::start(Arc::clone(&shared)).expect("runtime");
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (stopped_tx, stopped_rx) = std::sync::mpsc::channel();
        runtime
            .try_spawn(Box::pin(async move {
                let _stop = SendOnDrop(stopped_tx);
                started_tx.send(()).expect("receiver waiting");
                loop {
                    std::future::pending::<()>().await;
                }
            }))
            .expect("slot");
        started_rx.recv().expect("task starts");
        drop(runtime);
        stopped_rx.recv().expect("shutdown drops the resident task");
        assert!(shared.is_healthy(), "a cancelled task is not a panic");
    }

    #[test]
    fn test_saturated_runtime_refuses_further_tasks() {
        let shared = Arc::new(WorkerShared::new(1, 1));
        let runtime = AvailabilityRuntime::start(Arc::clone(&shared)).expect("runtime");
        let (release, held) = tokio::sync::oneshot::channel::<()>();
        let resident = runtime
            .try_spawn(Box::pin(async move {
                let _ = held.await;
            }))
            .expect("slot");
        assert!(
            runtime.try_spawn(Box::pin(std::future::ready(()))).is_none(),
            "a full runtime refuses further work"
        );
        assert_eq!(shared.rejected.load(Ordering::Relaxed), 1);
        release.send(()).expect("resident still waiting");
        runtime.handle.block_on(resident).expect("resident joins");
    }
}
