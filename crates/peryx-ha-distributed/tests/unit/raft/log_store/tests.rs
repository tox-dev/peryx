use openraft::StorageError;
use openraft::storage::RaftLogStorage;
use openraft::testing::{StoreBuilder, Suite, log_id};
use tempfile::TempDir;

use peryx_storage::raft::RaftLogStore;

use super::{NodeId, RaftLogStoreAdapter, TypeConfig};
use crate::raft::OwnershipStateMachine;

/// Builds a fresh redb-backed log adapter and the ownership state machine for the `openraft`
/// storage conformance suite. The `TempDir` guard keeps the database file alive for the test.
struct Builder;

impl StoreBuilder<TypeConfig, RaftLogStoreAdapter, OwnershipStateMachine, TempDir> for Builder {
    async fn build(&self) -> Result<(TempDir, RaftLogStoreAdapter, OwnershipStateMachine), StorageError<NodeId>> {
        let dir = tempfile::tempdir().unwrap();
        let store = RaftLogStore::open(dir.path().join("raft.redb")).unwrap();
        Ok((dir, RaftLogStoreAdapter::new(store), OwnershipStateMachine::default()))
    }
}

#[test]
fn test_passes_the_openraft_storage_conformance_suite() {
    Suite::test_all(Builder).unwrap();
}

#[tokio::test]
async fn test_committed_log_id_round_trips_and_clears() {
    let dir = tempfile::tempdir().unwrap();
    let mut adapter = RaftLogStoreAdapter::new(RaftLogStore::open(dir.path().join("raft.redb")).unwrap());
    assert_eq!(adapter.read_committed().await.unwrap(), None);

    let committed = log_id(2, 0, 7);
    adapter.save_committed(Some(committed)).await.unwrap();
    assert_eq!(adapter.read_committed().await.unwrap(), Some(committed));

    adapter.save_committed(None).await.unwrap();
    assert_eq!(adapter.read_committed().await.unwrap(), None);
}

#[tokio::test]
async fn test_a_corrupt_persisted_value_surfaces_a_storage_error() {
    let dir = tempfile::tempdir().unwrap();
    let store = RaftLogStore::open(dir.path().join("raft.redb")).unwrap();
    // A byte string the vote decoder cannot parse, written under the store's own vote key.
    store.save_vote(b"not valid json").unwrap();
    let mut adapter = RaftLogStoreAdapter::new(store);

    let error = adapter.read_vote().await.unwrap_err();

    // The serde decode failure rides with our wrapper: "Store" is the subject
    // label on every error this adapter renders, so match the decoder's own message instead.
    assert!(error.to_string().contains("expected"), "{error}");
}

#[tokio::test]
async fn test_a_store_fault_surfaces_as_a_storage_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bare.redb");
    // A valid redb database without the raft tables: a read hits redb's "table does not exist"
    // fault, which the adapter folds through its store-error arm into a storage error rather than
    // panicking.
    redb::Database::create(&path).unwrap();
    let mut adapter = RaftLogStoreAdapter::new(RaftLogStore::open_existing(&path).unwrap());

    let error = adapter.read_vote().await.unwrap_err();

    assert!(error.to_string().to_lowercase().contains("does not exist"), "{error}");
}
