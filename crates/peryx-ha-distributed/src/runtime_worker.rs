//! A bounded runtime that isolates availability work from foreground requests.
//!
//! Replicas use a dedicated Tokio runtime with capped worker, blocking, and task counts. Saturation
//! returns backpressure.
//!
use std::fmt::Write as _;
use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::{self, available_parallelism};

use futures_util::FutureExt as _;
use peryx_core::PrometheusSource;
use tokio::runtime::{Builder, Handle, Runtime};
use tokio::sync::oneshot;

use crate::lifecycle::Lifecycle;
use crate::{AnalyticsPuller, BeaconSender, ReplicaLoop};

#[cfg(peryx_loom)]
use loom::sync::atomic::AtomicUsize;
#[cfg(not(peryx_loom))]
use std::sync::atomic::AtomicUsize;

/// Caps background replication so host core counts cannot crowd foreground serving.
const WORKER_THREAD_CAP: usize = 4;

/// Bounds filesystem and checksum work outside the foreground executor.
const BLOCKING_THREAD_CAP: usize = 8;

/// The resident replica loop holds one slot for its lifetime.
const WORKER_SLOTS: usize = 32;

type BackgroundTask = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Honors CPU affinity before applying the worker cap.
fn worker_thread_count() -> usize {
    available_parallelism()
        .map_or(1, NonZeroUsize::get)
        .min(WORKER_THREAD_CAP)
}

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
    #[cfg(not(peryx_loom))]
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

    #[cfg(peryx_loom)]
    fn new(worker_threads: usize, total_slots: usize) -> Self {
        Self {
            worker_threads,
            total_slots,
            in_flight: AtomicUsize::new(0),
            rejected: AtomicU64::new(0),
            panics: AtomicU64::new(0),
            healthy: AtomicBool::new(true),
        }
    }

    #[must_use]
    pub fn for_replica() -> Self {
        Self::new(worker_thread_count(), WORKER_SLOTS)
    }

    /// A task panic marks the worker domain unhealthy without stopping reads.
    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    /// Returns `None` at capacity and records the rejected reservation.
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

pub struct AvailabilityRuntime {
    owner: Option<RuntimeOwner>,
    handle: Handle,
    shared: Arc<WorkerShared>,
    tasks: Mutex<Vec<TrackedTask>>,
}

struct RuntimeOwner {
    shutdown: mpsc::Sender<()>,
    thread: thread::JoinHandle<()>,
}

impl RuntimeOwner {
    fn start(worker_threads: usize) -> std::io::Result<(Self, Handle)> {
        let (shutdown, requested) = mpsc::channel();
        let runtime = Builder::new_multi_thread()
            .worker_threads(worker_threads)
            .max_blocking_threads(BLOCKING_THREAD_CAP)
            .thread_name("peryx-availability")
            .enable_all()
            .build()?;
        let handle = runtime.handle().clone();
        let thread = thread::Builder::new()
            .name("peryx-availability-owner".to_owned())
            .spawn(move || run_runtime_owner(&requested, runtime))?;
        Ok((Self { shutdown, thread }, handle))
    }

    fn shutdown(self) -> std::io::Result<()> {
        drop(self.shutdown);
        join_runtime_thread(self.thread)
    }
}

fn run_runtime_owner(requested: &mpsc::Receiver<()>, runtime: Runtime) {
    let _ = requested.recv();
    drop(runtime);
}

fn join_runtime_thread(thread: thread::JoinHandle<()>) -> std::io::Result<()> {
    match thread.join() {
        Ok(()) => Ok(()),
        Err(_) => Err(std::io::Error::other("availability runtime thread panicked")),
    }
}

fn reap_runtime_thread(thread: thread::JoinHandle<()>) {
    drop(crate::service_assembly::reap_process_resource(
        "availability runtime",
        move || join_runtime_thread(thread),
    ));
}

struct TrackedTask {
    abort: tokio::task::AbortHandle,
    completed: oneshot::Receiver<()>,
}

struct Completion {
    completed: Option<oneshot::Sender<()>>,
}

impl Drop for Completion {
    fn drop(&mut self) {
        if let Some(completed) = self.completed.take() {
            let _ = completed.send(());
        }
    }
}

impl AvailabilityRuntime {
    /// # Errors
    /// Returns the underlying error when the operating system refuses the worker threads.
    pub fn start(shared: Arc<WorkerShared>) -> std::io::Result<Self> {
        let (owner, handle) = RuntimeOwner::start(shared.worker_threads)?;
        Ok(Self {
            owner: Some(owner),
            handle,
            shared,
            tasks: Mutex::new(Vec::new()),
        })
    }

    /// Returns `None` at capacity. A panic releases the slot and fails worker health.
    pub fn try_spawn(&self, task: BackgroundTask) -> Option<tokio::task::JoinHandle<()>> {
        self.try_spawn_inner(task, None)
    }

    fn try_spawn_critical(
        &self,
        name: &'static str,
        task: BackgroundTask,
        lifecycle: Lifecycle,
    ) -> Option<tokio::task::JoinHandle<()>> {
        self.try_spawn_inner(task, Some((name, lifecycle)))
    }

    fn try_spawn_inner(
        &self,
        task: BackgroundTask,
        critical: Option<(&'static str, Lifecycle)>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let guard = self.shared.reserve()?;
        let shared = Arc::clone(&self.shared);
        let (completed, completion) = oneshot::channel();
        let task = self.handle.spawn(async move {
            let _guard = guard;
            let _completion = Completion {
                completed: Some(completed),
            };
            if let Some((_, lifecycle)) = &critical
                && !lifecycle.activated().await
            {
                return;
            }
            let panicked = std::panic::AssertUnwindSafe(task).catch_unwind().await.is_err();
            if panicked {
                shared.record_panic();
            }
            if let Some((name, lifecycle)) = critical
                && !lifecycle.is_cancelled()
            {
                lifecycle.fail(if panicked {
                    format!("availability {name} worker panicked")
                } else {
                    format!("availability {name} worker stopped unexpectedly")
                });
            }
        });
        let mut tasks = self.tasks.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        tasks.retain_mut(|task| {
            matches!(
                task.completed.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            )
        });
        tasks.push(TrackedTask {
            abort: task.abort_handle(),
            completed: completion,
        });
        drop(tasks);
        Some(task)
    }

    /// # Errors
    /// Returns an error when the runtime cannot reserve a slot for a configured service.
    pub(crate) fn start_replica_services_with_lifecycle(
        self,
        replica: ReplicaLoop,
        analytics: Option<AnalyticsPuller>,
        beacon: Option<BeaconSender>,
        lifecycle: Lifecycle,
    ) -> std::io::Result<Self> {
        self.try_spawn_critical("replica", Box::pin(replica.run()), lifecycle.clone())
            .ok_or_else(|| std::io::Error::other("reserve the replica worker slot"))?;
        if let Some(analytics) = analytics {
            self.try_spawn_critical("analytics", Box::pin(analytics.run()), lifecycle.clone())
                .ok_or_else(|| std::io::Error::other("reserve the analytics worker slot"))?;
        }
        if let Some(beacon) = beacon {
            self.try_spawn_critical("beacon", Box::pin(beacon.run()), lifecycle)
                .ok_or_else(|| std::io::Error::other("reserve the beacon worker slot"))?;
        }
        Ok(self)
    }

    pub(crate) fn cancel_workers(&mut self) {
        for task in self
            .tasks
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
        {
            task.abort.abort();
        }
    }

    pub(crate) async fn stop_workers(&mut self) {
        let tasks = std::mem::take(self.tasks.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner));
        for task in &tasks {
            task.abort.abort();
        }
        for task in tasks {
            let _ = task.completed.await;
        }
    }

    pub(crate) fn terminate(mut self) -> std::io::Result<()> {
        self.owner
            .take()
            .expect("runtime owner exists until termination")
            .shutdown()
    }

    pub(crate) fn terminate_workers(mut self) -> std::io::Result<()> {
        self.cancel_workers();
        self.terminate()
    }

    /// # Errors
    /// Returns the runtime owner thread's join failure.
    pub async fn shutdown(mut self) -> std::io::Result<()> {
        self.stop_workers().await;
        self.terminate()
    }
}

impl Drop for AvailabilityRuntime {
    fn drop(&mut self) {
        self.cancel_workers();
        if let Some(owner) = self.owner.take() {
            let RuntimeOwner { shutdown, thread } = owner;
            drop(shutdown);
            reap_runtime_thread(thread);
        }
    }
}

#[cfg(test)]
#[cfg(peryx_loom)]
#[path = "../tests/unit/runtime_worker/loom_tests.rs"]
mod loom_tests;
#[cfg(test)]
#[path = "../tests/unit/runtime_worker/tests.rs"]
mod tests;
