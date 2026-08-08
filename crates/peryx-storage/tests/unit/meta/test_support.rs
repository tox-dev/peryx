use std::sync::Arc;

use redb::backends::InMemoryBackend;

use super::MetaStore;
use super::fault::{self, Fault};

/// A production-schema metadata store with deterministic backend failures.
#[derive(Debug)]
pub struct FaultStore {
    inner: Arc<InMemoryBackend>,
    fault: Arc<Fault>,
}

impl FaultStore {
    /// Create and initialize an in-memory store.
    #[must_use]
    pub fn new() -> Self {
        let (store, inner, fault) = fault::initialized();
        drop(store);
        Self { inner, fault }
    }

    /// Bypass redb's page cache so an armed fault reaches reads.
    #[must_use]
    pub fn reopen(&self) -> MetaStore {
        fault::reopen(&self.inner, &self.fault)
    }

    /// Fail after `after` backend operations.
    pub fn arm(&self, after: i64) {
        self.fault.arm(after);
    }

    /// Stop injecting failures.
    pub fn disable(&self) {
        self.fault.disable();
    }
}

impl Default for FaultStore {
    fn default() -> Self {
        Self::new()
    }
}
