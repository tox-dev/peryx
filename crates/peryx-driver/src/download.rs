//! Neutral in-flight blob download coordination.
//!
//! A cold file request starts one detached transfer into a temp file; every concurrent request for
//! the same digest attaches to that transfer and tails its bytes as they land. The handle and its
//! progress are ecosystem-neutral: they carry a digest's bytes, not any package format, so they
//! live in the core serving crate and `AppState` holds the live-download registry. The ecosystem
//! driver owns only the format-specific decision of which digest to fetch.

use std::sync::Arc;

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use peryx_storage::blob::BlobTail;
use tokio::sync::watch;

/// Where one in-flight blob download stands; every client tailing it watches this value.
#[derive(Clone, Debug, Default)]
pub struct DownloadProgress {
    /// Bytes readable from the temp file so far.
    pub flushed: u64,
    /// Set once: `Ok` after the blob committed, `Err` when the transfer or verification failed.
    pub done: Option<Result<(), String>>,
}

/// A live download other requests for the same digest can attach to.
#[derive(Clone, Debug)]
pub struct DownloadHandle {
    tail: Option<BlobTail>,
    progress: watch::Receiver<DownloadProgress>,
}

impl DownloadHandle {
    /// Register a started transfer and its progress watch.
    #[must_use]
    pub fn new(tail: impl Into<Option<BlobTail>>, progress: watch::Receiver<DownloadProgress>) -> Self {
        Self {
            tail: tail.into(),
            progress,
        }
    }

    /// Opaque access to bytes flushed by the transfer.
    #[must_use]
    pub const fn tail(&self) -> Option<&BlobTail> {
        self.tail.as_ref()
    }

    /// The progress watch a tailing client reads and awaits changes on.
    #[must_use]
    pub const fn progress(&mut self) -> &mut watch::Receiver<DownloadProgress> {
        &mut self.progress
    }
}

/// Active downloads sharded by digest.
#[derive(Clone, Debug, Default)]
pub struct DownloadRegistry {
    entries: Arc<DashMap<Arc<str>, DownloadHandle>>,
}

impl DownloadRegistry {
    /// The active transfer for `digest`.
    #[must_use]
    pub fn get(&self, digest: &str) -> Option<DownloadHandle> {
        self.entries.get(digest).map(|entry| entry.value().clone())
    }

    /// Register a producer and return the client handle with its cancellation guard.
    ///
    /// # Errors
    /// Returns the existing handle when this digest already has a producer.
    pub fn register(
        &self,
        digest: &str,
        tail: impl Into<Option<BlobTail>>,
    ) -> Result<(DownloadHandle, DownloadProducer), DownloadHandle> {
        let digest = Arc::<str>::from(digest);
        match self.entries.entry(digest.clone()) {
            Entry::Occupied(entry) => Err(entry.get().clone()),
            Entry::Vacant(entry) => {
                let (sender, receiver) = watch::channel(DownloadProgress::default());
                let handle = DownloadHandle::new(tail, receiver);
                entry.insert(handle.clone());
                Ok((
                    handle,
                    DownloadProducer {
                        registry: self.clone(),
                        digest,
                        sender,
                        active: true,
                    },
                ))
            }
        }
    }
}

/// A registered download producer that removes and wakes its matching waiters on every exit path.
#[derive(Debug)]
pub struct DownloadProducer {
    registry: DownloadRegistry,
    digest: Arc<str>,
    sender: watch::Sender<DownloadProgress>,
    active: bool,
}

impl DownloadProducer {
    /// Bytes this producer has flushed for tailing clients.
    #[must_use]
    pub fn flushed(&self) -> u64 {
        self.sender.borrow().flushed
    }

    /// Publish a new flushed-byte boundary.
    pub fn publish_flushed(&self, flushed: u64) {
        self.sender.send_modify(|progress| progress.flushed = flushed);
    }

    /// Remove this producer and publish its terminal result.
    pub fn finish(mut self, outcome: Result<(), String>) {
        self.remove();
        self.active = false;
        self.sender.send_modify(|progress| progress.done = Some(outcome));
    }

    fn remove(&self) {
        self.registry.entries.remove(self.digest.as_ref());
    }
}

impl Drop for DownloadProducer {
    fn drop(&mut self) {
        if self.active {
            self.remove();
            self.sender.send_modify(|progress| {
                progress.done = Some(Err("blob transfer abandoned".to_owned()));
            });
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/download/tests.rs"]
mod tests;
