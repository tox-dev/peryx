//! Blank and membership entries carry no ownership command and return
//! [`NonMutating`](OwnershipResponse::NonMutating). Durable instances persist built and installed
//! snapshots, then reload state, membership, and `last_applied` after restart.

use std::io::Cursor;
use std::sync::Arc;

use openraft::storage::RaftStateMachine;
use openraft::{
    AnyError, Entry, EntryPayload, LogId, OptionalSend, RaftSnapshotBuilder, Snapshot, SnapshotMeta, StorageError,
    StorageIOError, StoredMembership,
};
use tokio::sync::Mutex;

use crate::ownership::{AppliedMeta, DatacenterId, OwnershipError, OwnershipState};
use crate::raft::persistence::{RaftLogError, RaftLogStore};
use crate::raft::{OwnershipResponse, PeryxNode, TypeConfig};
use crate::{Admission, AuthorityEpoch, AuthorityFence, AuthorityKey};
use peryx_ha::PendingTransferAudit;

type NodeId = u64;

/// Clones share applied state with snapshot builders.
#[derive(Debug, Clone, Default)]
pub struct OwnershipStateMachine {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Debug, Default)]
struct Inner {
    state: OwnershipState,
    last_applied: Option<LogId<NodeId>>,
    last_membership: StoredMembership<NodeId, PeryxNode>,
    snapshots_built: u64,
    current_snapshot: Option<StoredSnapshot>,
    snapshot_store: Option<RaftLogStore>,
}

#[derive(Debug, Clone)]
struct StoredSnapshot {
    meta: SnapshotMeta<NodeId, PeryxNode>,
    data: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
enum SnapshotStoreError {
    #[error(transparent)]
    Store(#[from] RaftLogError),
    #[error(transparent)]
    Codec(#[from] serde_json::Error),
    #[error(transparent)]
    Restore(#[from] OwnershipError),
}

impl From<SnapshotStoreError> for StorageError<NodeId> {
    fn from(error: SnapshotStoreError) -> Self {
        // `OpenRaft` turns every `StorageError` into `Fatal` and shuts down the node. This conversion
        // preserves the store, codec, or ownership failure for diagnostics.
        StorageIOError::read_snapshot(None, AnyError::new(&error)).into()
    }
}

impl Inner {
    /// Reloads state, membership, and `last_applied` after log compaction.
    fn load(store: RaftLogStore) -> Result<Self, SnapshotStoreError> {
        let mut inner = Self::default();
        if let Some(stored) = store.read_snapshot()? {
            let meta: SnapshotMeta<NodeId, PeryxNode> = serde_json::from_slice(&stored.meta)?;
            inner.state = OwnershipState::restore(&stored.data)?;
            inner.last_applied = meta.last_log_id;
            inner.last_membership = meta.last_membership.clone();
            inner.current_snapshot = Some(StoredSnapshot {
                meta,
                data: stored.data,
            });
        }
        inner.snapshot_store = Some(store);
        Ok(inner)
    }

    fn persist_snapshot(&self, meta: &SnapshotMeta<NodeId, PeryxNode>, data: &[u8]) -> Result<(), SnapshotStoreError> {
        if let Some(store) = &self.snapshot_store {
            let meta = serde_json::to_vec(meta).expect("a snapshot meta always serializes to JSON");
            store.save_snapshot(&meta, data)?;
        }
        Ok(())
    }
}

impl OwnershipStateMachine {
    /// Opens durable state and reloads its latest snapshot.
    ///
    /// # Errors
    /// Returns [`StorageError`] when store reads fail or snapshot restoration violates ownership
    /// invariants.
    pub fn with_snapshot_store(store: RaftLogStore) -> Result<Self, Box<StorageError<NodeId>>> {
        let inner = Inner::load(store).map_err(|error| Box::new(StorageError::from(error)))?;
        Ok(Self {
            inner: Arc::new(Mutex::new(inner)),
        })
    }

    /// Reads local applied state, which may lag on followers. The committed compare-and-set limits a
    /// stale `None` to a rejected assignment.
    pub async fn home_of(&self, authority: &AuthorityKey) -> Option<DatacenterId> {
        self.inner.lock().await.state.home(authority).cloned()
    }

    /// Read both fields under one lock so they belong to the same assignment.
    pub async fn home_claim(&self, authority: &AuthorityKey) -> Option<(DatacenterId, AuthorityEpoch)> {
        let inner = self.inner.lock().await;
        inner
            .state
            .home(authority)
            .cloned()
            .map(|home| (home, inner.state.epoch(authority)))
    }

    /// Reads local applied state, which may lag on followers. Writers stamp this epoch on work; nodes
    /// that have applied a newer epoch reject stale work.
    pub async fn epoch_of(&self, authority: &AuthorityKey) -> AuthorityEpoch {
        self.inner.lock().await.state.epoch(authority)
    }

    /// Audits sealed by a committed transfer that no projector has reported storing.
    pub async fn pending_transfer_audits(&self) -> Vec<PendingTransferAudit> {
        self.inner.lock().await.state.pending_transfer_audits()
    }

    /// Rejects work from a superseded epoch against this node's applied state.
    pub async fn admit(&self, authority: &AuthorityKey, presented: AuthorityEpoch) -> Admission {
        let mut fence = AuthorityFence::new();
        fence.commit(authority, self.inner.lock().await.state.epoch(authority));
        fence.admit(authority, presented)
    }
}

impl RaftSnapshotBuilder<TypeConfig> for OwnershipStateMachine {
    async fn build_snapshot(&mut self) -> Result<Snapshot<TypeConfig>, StorageError<NodeId>> {
        let mut inner = self.inner.lock().await;
        let data = inner.state.snapshot();
        inner.snapshots_built += 1;
        let last_index = inner.last_applied.map_or(0, |log_id| log_id.index);
        let snapshot_id = format!("{last_index}-{}", inner.snapshots_built);
        let meta = SnapshotMeta {
            last_log_id: inner.last_applied,
            last_membership: inner.last_membership.clone(),
            snapshot_id,
        };
        inner.persist_snapshot(&meta, &data)?;
        inner.current_snapshot = Some(StoredSnapshot {
            meta: meta.clone(),
            data: data.clone(),
        });
        drop(inner);
        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(data)),
        })
    }
}

impl RaftStateMachine<TypeConfig> for OwnershipStateMachine {
    type SnapshotBuilder = Self;

    async fn applied_state(
        &mut self,
    ) -> Result<(Option<LogId<NodeId>>, StoredMembership<NodeId, PeryxNode>), StorageError<NodeId>> {
        let inner = self.inner.lock().await;
        Ok((inner.last_applied, inner.last_membership.clone()))
    }

    async fn apply<I>(&mut self, entries: I) -> Result<Vec<OwnershipResponse>, StorageError<NodeId>>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + OptionalSend,
        I::IntoIter: OptionalSend,
    {
        let mut inner = self.inner.lock().await;
        let mut responses = Vec::new();
        for entry in entries {
            inner.last_applied = Some(entry.log_id);
            let meta = AppliedMeta {
                term: entry.log_id.leader_id.term,
                index: entry.log_id.index,
            };
            let response = match entry.payload {
                EntryPayload::Blank => OwnershipResponse::NonMutating,
                EntryPayload::Normal(command) => OwnershipResponse::Applied(inner.state.apply(&command, meta)),
                EntryPayload::Membership(membership) => {
                    inner.last_membership = StoredMembership::new(Some(entry.log_id), membership);
                    OwnershipResponse::NonMutating
                }
            };
            responses.push(response);
        }
        drop(inner);
        Ok(responses)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }

    async fn begin_receiving_snapshot(&mut self) -> Result<Box<Cursor<Vec<u8>>>, StorageError<NodeId>> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<NodeId, PeryxNode>,
        snapshot: Box<Cursor<Vec<u8>>>,
    ) -> Result<(), StorageError<NodeId>> {
        let data = snapshot.into_inner();
        let state = OwnershipState::restore(&data)
            .map_err(|error| StorageIOError::read_snapshot(Some(meta.signature()), AnyError::new(&error)))?;
        let mut inner = self.inner.lock().await;
        inner.persist_snapshot(meta, &data)?;
        inner.state = state;
        inner.last_applied = meta.last_log_id;
        inner.last_membership = meta.last_membership.clone();
        inner.current_snapshot = Some(StoredSnapshot {
            meta: meta.clone(),
            data,
        });
        drop(inner);
        Ok(())
    }

    async fn get_current_snapshot(&mut self) -> Result<Option<Snapshot<TypeConfig>>, StorageError<NodeId>> {
        let inner = self.inner.lock().await;
        Ok(inner.current_snapshot.as_ref().map(|stored| Snapshot {
            meta: stored.meta.clone(),
            snapshot: Box::new(Cursor::new(stored.data.clone())),
        }))
    }
}
