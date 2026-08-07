//! The `OpenRaft` state machine that applies committed ownership log entries.
//!
//! [`OwnershipStateMachine`] binds the pure [`OwnershipState`] to `OpenRaft`'s
//! [`RaftStateMachine`] contract. `OpenRaft` hands it the committed entries in order, and it folds each
//! [`Normal`](openraft::EntryPayload::Normal) entry's [`OwnershipCommand`](crate::OwnershipCommand)
//! through [`OwnershipState::apply`], returning the [`OwnershipEffect`](crate::OwnershipEffect) wrapped
//! as an [`OwnershipResponse`]. The blank entry a new leader commits and the membership entries `OpenRaft`
//! manages carry no command; it records the membership and reports them as
//! [`NonMutating`](OwnershipResponse::NonMutating).
//!
//! Snapshots ride on the pure state's own format: [`OwnershipState::snapshot`] produces the bytes and
//! [`OwnershipState::restore`] rebuilds from them, so the same one-based, zero-reserved epoch invariant
//! the state enforces survives a snapshot install. The state lives behind an `Arc<Mutex<_>>` so the
//! snapshot builder shares one view with the applier.
//!
//! A machine built with [`with_snapshot_store`](OwnershipStateMachine::with_snapshot_store) persists every
//! snapshot it builds or installs into the durable [`RaftLogStore`] and reloads the latest one on startup,
//! so a restart after log compaction recovers the ownership map, epochs, membership, and `last_applied`
//! from the stored snapshot rather than the purged log. A machine built with [`Default`] keeps its
//! snapshot in memory only, which the `openraft` conformance suite and the pure-logic tests exercise.

use std::io::Cursor;
use std::sync::Arc;

use openraft::storage::RaftStateMachine;
use openraft::{
    AnyError, Entry, EntryPayload, LogId, OptionalSend, RaftSnapshotBuilder, Snapshot, SnapshotMeta, StorageError,
    StorageIOError, StoredMembership,
};
use peryx_storage::raft::{RaftLogError, RaftLogStore};
use tokio::sync::Mutex;

use crate::ownership::{AppliedMeta, DatacenterId, OwnershipError, OwnershipState};
use crate::raft::{OwnershipResponse, PeryxNode, TypeConfig};
use crate::{Admission, AuthorityEpoch, AuthorityFence, AuthorityKey};

/// The `u64` voter handle `OpenRaft` keys nodes by. See [`crate::raft`] for why it is not the datacenter
/// identity.
type NodeId = u64;

/// The replicated ownership state machine driven by `OpenRaft`.
///
/// Cloning shares the underlying state, so the snapshot builder returned by
/// [`get_snapshot_builder`](RaftStateMachine::get_snapshot_builder) reads the same state the applier
/// writes.
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
    /// The durable store to persist snapshots into and reload them from, or `None` for an in-memory-only
    /// machine. Shared through the same redb database the log adapter writes, so a node keeps one store.
    snapshot_store: Option<RaftLogStore>,
}

/// The most recent snapshot this machine built or installed, kept so
/// [`get_current_snapshot`](RaftStateMachine::get_current_snapshot) can return it.
#[derive(Debug, Clone)]
struct StoredSnapshot {
    meta: SnapshotMeta<NodeId, PeryxNode>,
    data: Vec<u8>,
}

/// A rejected durable snapshot operation before it is mapped into `openraft`'s storage error: the store
/// failed, a persisted blob would not decode, or a restored state broke the ownership invariant.
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
        // `openraft` folds every `StorageError` into `Fatal` and shuts the node down; the subject and
        // signature are diagnostic, never a retry-vs-shutdown signal. So each fallible snapshot store
        // operation maps through this one conversion, keeping the sole error-formatting path tested while
        // the underlying redb, serde, or ownership error rides along.
        StorageIOError::read_snapshot(None, AnyError::new(&error)).into()
    }
}

impl Inner {
    /// Reopen `store` and fold its persisted snapshot, if any, back into a fresh machine's applied state so
    /// a restart recovers the ownership map, epochs, membership, and `last_applied` from the stored
    /// snapshot rather than the compacted log.
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

    /// Persist `meta` and `data` as the current durable snapshot, a no-op on an in-memory-only machine.
    fn persist_snapshot(&self, meta: &SnapshotMeta<NodeId, PeryxNode>, data: &[u8]) -> Result<(), SnapshotStoreError> {
        if let Some(store) = &self.snapshot_store {
            let meta = serde_json::to_vec(meta).expect("a snapshot meta always serializes to JSON");
            store.save_snapshot(&meta, data)?;
        }
        Ok(())
    }
}

impl OwnershipStateMachine {
    /// Build a machine backed by the durable `store`, reloading the persisted snapshot so its applied
    /// state survives a process restart after log compaction.
    ///
    /// # Errors
    /// A [`StorageError`] when the store cannot be read or its persisted snapshot will not decode into a
    /// valid ownership state. Boxed because `openraft`'s storage error is large and this is the one
    /// caller-facing (non-trait) result that returns it.
    pub fn with_snapshot_store(store: RaftLogStore) -> Result<Self, Box<StorageError<NodeId>>> {
        let inner = Inner::load(store).map_err(|error| Box::new(StorageError::from(error)))?;
        Ok(Self {
            inner: Arc::new(Mutex::new(inner)),
        })
    }

    /// The datacenter that homes `authority` in this node's applied ownership state, or `None` when no
    /// committed command has homed it.
    ///
    /// A local read: current on the leader, possibly behind on a follower. It gates work a committed
    /// compare-and-set still settles without reassigning an already-homed authority, so a stale `None`
    /// costs one rejected assignment, never a wrong home.
    pub async fn home_of(&self, authority: &AuthorityKey) -> Option<DatacenterId> {
        self.inner.lock().await.state.home(authority).cloned()
    }

    /// The committed authority epoch of `authority` in this node's applied ownership state, or the
    /// unassigned sentinel [`AuthorityEpoch(0)`](crate::AuthorityEpoch) when no committed command has
    /// homed it.
    ///
    /// A local read like [`home_of`](Self::home_of): current on the leader, possibly behind on a
    /// follower. It is the fence value a writer stamps onto work it produces, so a former holder's stale
    /// epoch is fenced out once the authority advances.
    pub async fn epoch_of(&self, authority: &AuthorityKey) -> AuthorityEpoch {
        self.inner.lock().await.state.epoch(authority)
    }

    /// Whether work carrying `presented` under `authority` may proceed against this node's committed
    /// epoch, or is fenced as stale.
    ///
    /// Feeds an [`AuthorityFence`] from the applied ownership state and admits `presented`, so work
    /// produced under a superseded epoch - a former holder's, after the authority advanced - is
    /// [`Fenced`](Admission::Fenced) without effect.
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
