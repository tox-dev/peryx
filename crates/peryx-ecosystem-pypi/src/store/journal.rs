use serde::{Deserialize, Serialize};

/// One recorded mutation in the [`MetaStore`] journal: the append-only changelog that makes peryx
/// an origin others can replicate from. `serial` orders entries; the rest names what changed.
///
/// The neutral serial counter lives in the store, so a `PyPI` publish builds this entry with a
/// placeholder `serial` and lets [`commit_driver_batch_journaled`] allocate the authoritative one —
/// see [`publish_file`](super::publish_file).
///
/// [`MetaStore`]: peryx_storage::meta::MetaStore
/// [`commit_driver_batch_journaled`]: peryx_storage::meta::MetaStore::commit_driver_batch_journaled
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub serial: u64,
    pub action: String,
    pub project: String,
    pub version: Option<String>,
    pub filename: Option<String>,
}
