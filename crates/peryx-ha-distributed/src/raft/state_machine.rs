//! Blank and membership entries carry no ownership command and return
//! [`NonMutating`](OwnershipResponse::NonMutating). Durable instances persist built and installed
//! snapshots, then reload state, membership, `last_applied`, and the build generation after restart.
//! The generation names snapshots, so a replacement process never repeats an identifier a follower
//! may still be assembling chunks under.
//!
//! Serialization, restore and the redb commit run on a blocking thread with the state lock dropped,
//! so a slow disk delays neither ownership reads nor log application. Candidates carry a
//! [`SnapshotRank`], and both the durable record and `current_snapshot` keep the highest-ranked one,
//! so a task that finishes late cannot replace a newer snapshot.

use std::collections::BTreeSet;
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

// A child module, so the tests reach the candidate seam without any of it becoming crate-visible.
#[cfg(test)]
#[path = "../../tests/unit/raft/state_machine_tests.rs"]
mod tests;

type NodeId = u64;

/// Clones share applied state with snapshot builders.
#[derive(Debug, Clone, Default)]
pub struct OwnershipStateMachine {
    inner: Arc<Mutex<Inner>>,
    /// Serializes snapshot commits and records the highest rank the store holds. Snapshot work
    /// alone waits here, so a commit in flight never delays a read of `inner`.
    durable: Arc<Mutex<Option<SnapshotRank>>>,
}

/// Orders snapshot candidates. The log position decides, and the publication number breaks a tie
/// between two candidates taken at the same position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SnapshotRank {
    last_log_id: Option<LogId<NodeId>>,
    publication: u64,
}

/// One consistent state and the identity it was named under, taken while holding the state lock so
/// the work that follows needs neither.
#[derive(Debug)]
struct SnapshotCandidate {
    state: OwnershipState,
    meta: SnapshotMeta<NodeId, PeryxNode>,
    rank: SnapshotRank,
    generation: u64,
    store: Option<RaftLogStore>,
}

#[derive(Debug, Default)]
struct Inner {
    state: OwnershipState,
    last_applied: Option<LogId<NodeId>>,
    last_membership: StoredMembership<NodeId, PeryxNode>,
    /// Builds this store has named. Durable, because a counter held only in the process restarts at
    /// zero and re-issues an identifier a peer may still be streaming chunks under.
    snapshot_generation: u64,
    /// Numbers every candidate this process takes, so two in flight at the same log position still
    /// order against each other.
    publications: u64,
    published: Option<SnapshotRank>,
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
    /// Reloads state, membership, `last_applied`, and the snapshot generation after log compaction.
    fn load(store: RaftLogStore) -> Result<Self, SnapshotStoreError> {
        let mut inner = Self {
            snapshot_generation: store.read_snapshot_generation()?,
            ..Self::default()
        };
        if let Some(stored) = store.read_snapshot()? {
            let meta: SnapshotMeta<NodeId, PeryxNode> = serde_json::from_slice(&stored.meta)?;
            inner.state = OwnershipState::restore(&stored.data)?;
            inner.last_applied = meta.last_log_id;
            inner.last_membership = meta.last_membership.clone();
            let projectors = audit_projectors(&inner.last_membership);
            inner.state.set_audit_projectors(projectors);
            inner.published = Some(SnapshotRank {
                last_log_id: meta.last_log_id,
                publication: 0,
            });
            inner.current_snapshot = Some(StoredSnapshot {
                meta,
                data: stored.data,
            });
        }
        inner.snapshot_store = Some(store);
        Ok(inner)
    }

    /// Numbers the next candidate. Callers hold the lock, so the numbers follow the order the
    /// candidates read the state in.
    const fn next_rank(&mut self, last_log_id: Option<LogId<NodeId>>) -> SnapshotRank {
        self.publications += 1;
        SnapshotRank {
            last_log_id,
            publication: self.publications,
        }
    }

    /// Keeps the highest-ranked snapshot, so a task that finishes after a newer one cannot replace
    /// it.
    fn publish(&mut self, rank: SnapshotRank, meta: SnapshotMeta<NodeId, PeryxNode>, data: Vec<u8>) {
        if self.published.is_some_and(|published| rank <= published) {
            return;
        }
        self.published = Some(rank);
        self.current_snapshot = Some(StoredSnapshot { meta, data });
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
            durable: Arc::new(Mutex::new(None)),
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

    /// Audits sealed by a committed transfer that `projector` has not reported storing.
    pub async fn pending_transfer_audits(&self, projector: &str) -> Vec<PendingTransferAudit> {
        self.inner.lock().await.state.pending_transfer_audits(projector)
    }

    /// Rejects work from a superseded epoch against this node's applied state.
    pub async fn admit(&self, authority: &AuthorityKey, presented: AuthorityEpoch) -> Admission {
        let mut fence = AuthorityFence::new();
        fence.commit(authority, self.inner.lock().await.state.epoch(authority));
        fence.admit(authority, presented)
    }

    /// Clones one consistent state and names it, so serialization and the commit run without the
    /// state lock and later applies can proceed against the live state.
    async fn snapshot_candidate(&self) -> SnapshotCandidate {
        let mut inner = self.inner.lock().await;
        inner.snapshot_generation += 1;
        let last_applied = inner.last_applied;
        let last_index = last_applied.map_or(0, |log_id| log_id.index);
        let snapshot_id = format!("{last_index}-{}", inner.snapshot_generation);
        let rank = inner.next_rank(last_applied);
        SnapshotCandidate {
            state: inner.state.clone(),
            meta: SnapshotMeta {
                last_log_id: last_applied,
                last_membership: inner.last_membership.clone(),
                snapshot_id,
            },
            rank,
            generation: inner.snapshot_generation,
            store: inner.snapshot_store.clone(),
        }
    }

    /// Serializes and commits a candidate, then publishes it unless a newer one already stands.
    async fn store_candidate(
        &self,
        candidate: SnapshotCandidate,
    ) -> Result<Snapshot<TypeConfig>, StorageError<NodeId>> {
        let SnapshotCandidate {
            state,
            meta,
            rank,
            generation,
            store,
        } = candidate;
        let encoded = serde_json::to_vec(&meta).expect("a snapshot meta always serializes to JSON");
        let data = self
            .commit_snapshot(rank, move |superseded| {
                let data = state.snapshot();
                // The generation reaches the store before the identifier reaches `OpenRaft`, so a
                // crash between the two loses the snapshot rather than leaking a name a restart
                // could repeat.
                if let Some(store) = store.filter(|_| !superseded) {
                    store.save_snapshot(&encoded, &data, generation)?;
                }
                Ok(data)
            })
            .await?;
        self.inner.lock().await.publish(rank, meta.clone(), data.clone());
        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(data)),
        })
    }

    /// Runs `commit` on a blocking thread with the state lock already released, so neither a
    /// Tokio worker nor an ownership read waits on the disk. `commit` receives whether a
    /// higher-ranked candidate already reached the store and skips its write when it has, and the
    /// recorded rank advances only once the write has landed.
    async fn commit_snapshot<T>(
        &self,
        rank: SnapshotRank,
        commit: impl FnOnce(bool) -> Result<T, SnapshotStoreError> + Send + 'static,
    ) -> Result<T, SnapshotStoreError>
    where
        T: Send + 'static,
    {
        let mut durable = self.durable.lock().await;
        let superseded = durable.is_some_and(|written| rank <= written);
        let value = tokio::task::spawn_blocking(move || commit(superseded))
            .await
            .expect("the snapshot commit task runs to completion")?;
        if !superseded {
            *durable = Some(rank);
        }
        drop(durable);
        Ok(value)
    }
}

impl RaftSnapshotBuilder<TypeConfig> for OwnershipStateMachine {
    async fn build_snapshot(&mut self) -> Result<Snapshot<TypeConfig>, StorageError<NodeId>> {
        let candidate = self.snapshot_candidate().await;
        self.store_candidate(candidate).await
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
            let response = match entry.payload {
                EntryPayload::Blank => OwnershipResponse::NonMutating,
                EntryPayload::Normal(command) => OwnershipResponse::Applied(inner.state.apply(
                    &command,
                    AppliedMeta {
                        term: entry.log_id.leader_id.term,
                        index: entry.log_id.index,
                    },
                )),
                EntryPayload::Membership(membership) => {
                    inner.last_membership = StoredMembership::new(Some(entry.log_id), membership);
                    let projectors = audit_projectors(&inner.last_membership);
                    inner.state.set_audit_projectors(projectors);
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
        let (rank, generation, store) = {
            let mut inner = self.inner.lock().await;
            (
                inner.next_rank(meta.last_log_id),
                inner.snapshot_generation,
                inner.snapshot_store.clone(),
            )
        };
        let projectors = audit_projectors(&meta.last_membership);
        let encoded = serde_json::to_vec(meta).expect("a snapshot meta always serializes to JSON");
        // Restore precedes the write, so a corrupt snapshot never reaches the store.
        let (state, data) = self
            .commit_snapshot(rank, move |superseded| {
                let mut state = OwnershipState::restore(&data)?;
                state.set_audit_projectors(projectors);
                if let Some(store) = store.filter(|_| !superseded) {
                    store.save_snapshot(&encoded, &data, generation)?;
                }
                Ok((state, data))
            })
            .await
            .map_err(|error| match error {
                SnapshotStoreError::Restore(error) => {
                    StorageIOError::read_snapshot(Some(meta.signature()), AnyError::new(&error)).into()
                }
                error => StorageError::from(error),
            })?;
        // One critical section, so a reader sees the state it had before the install or the whole
        // restored state, never a partial one.
        let mut inner = self.inner.lock().await;
        inner.state = state;
        inner.last_applied = meta.last_log_id;
        inner.last_membership = meta.last_membership.clone();
        inner.publish(rank, meta.clone(), data);
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

fn audit_projectors(membership: &StoredMembership<NodeId, PeryxNode>) -> BTreeSet<String> {
    let voters: BTreeSet<NodeId> = membership.voter_ids().collect();
    membership
        .nodes()
        .filter(|(id, _)| voters.contains(id))
        .map(|(_, node)| node.datacenter.0.clone())
        .collect()
}
