//! Neutral in-flight blob download coordination.
//!
//! A cold file request starts one detached transfer into a temp file; every concurrent request for
//! the same digest attaches to that transfer and tails its bytes as they land. The handle and its
//! progress are ecosystem-neutral: they carry a digest's bytes, not any package format, so they
//! live in the core serving crate and `AppState` holds the live-download registry. The ecosystem
//! driver owns only the format-specific decision of which digest to fetch.

use std::path::{Path, PathBuf};

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
    path: PathBuf,
    progress: watch::Receiver<DownloadProgress>,
}

impl DownloadHandle {
    /// Register a started transfer landing in `path`, watched through `progress`.
    #[must_use]
    pub const fn new(path: PathBuf, progress: watch::Receiver<DownloadProgress>) -> Self {
        Self { path, progress }
    }

    /// The temp file the transfer lands in until commit renames it.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The progress watch a tailing client reads and awaits changes on.
    #[must_use]
    pub const fn progress(&mut self) -> &mut watch::Receiver<DownloadProgress> {
        &mut self.progress
    }
}
