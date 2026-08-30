//! Runs a request's synchronous metadata scan on a blocking worker.
//!
//! A full scan is CPU- and IO-bound with no await points, so running it on a Tokio request worker
//! holds that worker for the whole scan and delays unrelated requests. Moving it to a blocking worker
//! frees the async pool, but a blocking task cannot be aborted once it has started, so two properties
//! have to be arranged here rather than left to the runtime: a dropped request signals the scan
//! cooperatively instead of stopping it, and the concurrency slot stays held until the worker returns,
//! so a burst of abandoned requests cannot oversubscribe the blocking pool.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Semaphore;

/// A cooperative stop signal handed to a scan running on a blocking worker.
#[derive(Clone, Default)]
pub struct ScanCancellation {
    cancelled: Arc<AtomicBool>,
}

impl ScanCancellation {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

/// The shared admission bound for request scans that run on blocking workers.
#[derive(Clone)]
pub struct BlockingScanExecutor {
    permits: Arc<Semaphore>,
}

impl BlockingScanExecutor {
    #[must_use]
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(capacity)),
        }
    }

    /// Runs synchronous scan work without occupying an async worker.
    ///
    /// The closure's cancellation signal changes when the awaiting request drops. A started worker
    /// keeps its permit until `work` returns because Tokio cannot abort a started [`spawn_blocking`]
    /// task.
    ///
    /// [`spawn_blocking`]: https://docs.rs/tokio/1.53.1/tokio/task/fn.spawn_blocking.html
    ///
    /// # Panics
    ///
    /// Panics only if the private semaphore is closed. No code path closes it.
    ///
    /// # Errors
    /// Returns a join error if the blocking task panics or the runtime stops it before it starts.
    pub async fn run<T, F>(&self, work: F) -> Result<T, tokio::task::JoinError>
    where
        T: Send + 'static,
        F: FnOnce(&ScanCancellation) -> T + Send + 'static,
    {
        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .expect("the blocking scan semaphore remains open");
        let cancellation = ScanCancellation::new();
        let cancel_on_drop = CancelOnDrop(cancellation.clone());
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            work(&cancellation)
        })
        .await;
        drop(cancel_on_drop);
        result
    }
}

struct CancelOnDrop(ScanCancellation);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[cfg(test)]
#[path = "../tests/unit/blocking_scan/tests.rs"]
mod tests;
