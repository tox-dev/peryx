//! Every streamed write, materialized download, and multipart journal passes through a path the store
//! must be able to tell apart from a published blob. A shared prefix makes an abandoned stage
//! recognizable, and an ownership registry keeps a sweep off the ones this process still drives.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Marks a temporary as unpublished, so a sweep never confuses one with a resident blob.
pub const STAGE_PREFIX: &str = ".peryx-stage-";

/// Ownership already protects the writes this process drives, so the age bound only has to outlast a
/// write another process on the same store is still streaming.
pub const STAGE_MAX_AGE: Duration = Duration::from_hours(24);

pub fn is_stage(name: &OsStr) -> bool {
    name.as_encoded_bytes().starts_with(STAGE_PREFIX.as_bytes())
}

/// Paths a write in this process still drives, counted because concurrent writers of one blob share a
/// path. Recovery treats every path nothing here owns as abandoned.
#[derive(Debug, Default)]
pub struct PathOwners(std::sync::Mutex<HashMap<PathBuf, usize>>);

impl PathOwners {
    pub(crate) fn own(self: &Arc<Self>, path: PathBuf) -> OwnedPath {
        *self.owned().entry(path.clone()).or_default() += 1;
        OwnedPath {
            owners: Arc::clone(self),
            path,
        }
    }

    pub(crate) fn owns(&self, path: &Path) -> bool {
        self.owned().contains_key(path)
    }

    fn owned(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, usize>> {
        self.0.lock().expect("path ownership is never held across a panic")
    }
}

#[derive(Debug)]
pub struct OwnedPath {
    owners: Arc<PathOwners>,
    path: PathBuf,
}

impl Drop for OwnedPath {
    fn drop(&mut self) {
        let mut owned = self.owners.owned();
        let remaining = owned
            .get_mut(&self.path)
            .expect("an owned path stays registered until its guard drops");
        *remaining -= 1;
        if *remaining == 0 {
            owned.remove(&self.path);
        }
    }
}

/// Bytes that staged writes left behind, reported apart from resident blobs so a leak is attributable.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StageUsage {
    pub files: u64,
    pub bytes: u64,
}

#[cfg(test)]
#[path = "../../tests/unit/blob/stage/tests.rs"]
mod tests;
