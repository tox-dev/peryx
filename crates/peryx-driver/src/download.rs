//! Shared transfer state belongs here because ecosystem drivers only select the digest.

use std::sync::Arc;

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use peryx_storage::blob::BlobTail;
use tokio::sync::watch;

/// Observable state for an in-flight blob transfer.
#[derive(Clone, Debug, Default)]
pub struct DownloadProgress {
    /// Bytes readable from the temp file so far.
    pub flushed: u64,
    /// Set once: `Ok` after the blob committed, `Err` when the transfer or verification failed.
    pub done: Option<Result<(), String>>,
}

/// A transfer that concurrent requests for the same digest can follow.
#[derive(Clone, Debug)]
pub struct DownloadHandle {
    tail: Option<BlobTail>,
    progress: watch::Receiver<DownloadProgress>,
}

impl DownloadHandle {
    #[must_use]
    pub fn new(tail: impl Into<Option<BlobTail>>, progress: watch::Receiver<DownloadProgress>) -> Self {
        Self {
            tail: tail.into(),
            progress,
        }
    }

    #[must_use]
    pub const fn tail(&self) -> Option<&BlobTail> {
        self.tail.as_ref()
    }

    #[must_use]
    pub const fn progress(&mut self) -> &mut watch::Receiver<DownloadProgress> {
        &mut self.progress
    }
}

/// Active transfers keyed by digest.
#[derive(Clone, Debug, Default)]
pub struct DownloadRegistry {
    entries: Arc<DashMap<Arc<str>, DownloadHandle>>,
}

impl DownloadRegistry {
    #[must_use]
    pub fn get(&self, digest: &str) -> Option<DownloadHandle> {
        self.entries.get(digest).map(|entry| entry.value().clone())
    }

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

/// A producer that removes its registration and wakes waiters on every exit path.
#[derive(Debug)]
pub struct DownloadProducer {
    registry: DownloadRegistry,
    digest: Arc<str>,
    sender: watch::Sender<DownloadProgress>,
    active: bool,
}

impl DownloadProducer {
    #[must_use]
    pub fn flushed(&self) -> u64 {
        self.sender.borrow().flushed
    }

    pub fn publish_flushed(&self, flushed: u64) {
        self.sender.send_modify(|progress| progress.flushed = flushed);
    }

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
