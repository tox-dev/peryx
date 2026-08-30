//! Every hosted (pushed) metadata change appends one [`OciMutation`] to the journal in the same
//! [`commit_driver_txn`](peryx_storage::meta::MetaStore::commit_driver_txn) that writes its rows, so
//! the two commit together and a replica reconciles the change exactly once, in serial order. Proxy
//! cache fills and freshness carry no entry: a replica reconstructs them by pulling upstream, so
//! journaling them would replicate derived state. A node journals only in an enabled availability
//! mode; under single-node `none` the outbox stays empty and the write count is unchanged.

use serde::{Deserialize, Serialize};

pub type Outbox = bool;

/// One authoritative OCI repository mutation, the payload of an outbox journal entry.
///
/// Manifests are content-addressed and immutable, so publishing one and retargeting a tag are
/// distinct operations: repointing a tag changes no bytes but is a mutation a replica applies in
/// order. Deletions are soft - a delete moves the reference into repository trash - so the trash and
/// restore transitions are the deletion vocabulary a replica replays. A blob delete is the exception:
/// the distribution spec drops the repository link outright, so it carries its own removal operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum OciMutation {
    /// A manifest published under `(index, repo)`, retargeting `tag` when the push named one.
    PublishManifest {
        index: String,
        repo: String,
        digest: String,
        tag: Option<String>,
    },
    /// A blob admitted to `(index, repo)` membership, by a hosted push or a cross-repo mount.
    MountBlob {
        index: String,
        repo: String,
        digest: String,
    },
    /// A blob's `(index, repo)` membership removed by a hosted delete. The bytes stay: another
    /// repository that links the same digest keeps serving it.
    UnmountBlob {
        index: String,
        repo: String,
        digest: String,
    },
    /// A tag moved into repository trash, capturing the digest it pointed at.
    TrashTag {
        index: String,
        repo: String,
        tag: String,
        digest: String,
    },
    /// A digest and every tag that pointed at it moved into repository trash together.
    TrashManifest {
        index: String,
        repo: String,
        digest: String,
        tags: Vec<String>,
    },
    /// A trashed tag restored to its captured digest.
    RestoreTag {
        index: String,
        repo: String,
        tag: String,
        digest: String,
    },
    /// A trashed digest restored, relighting each captured tag whose live slot was free.
    RestoreManifest {
        index: String,
        repo: String,
        digest: String,
        tags: Vec<String>,
    },
}

impl OciMutation {
    /// Serialize as the outbox entry bytes. The store allocates the authoritative serial when it
    /// commits, so the payload carries only the operation.
    fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("an OCI mutation always serializes")
    }
}

pub fn record(outbox: Outbox, op: impl FnOnce() -> OciMutation) -> Vec<Vec<u8>> {
    if outbox { vec![op().encode()] } else { Vec::new() }
}
