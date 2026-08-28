use std::error::Error as _;

use openraft::storage::{RaftLogReader, RaftLogStorage, RaftLogStorageExt};
use openraft::testing::{StoreBuilder, Suite, log_id};
use openraft::{Entry, EntryPayload, ErrorSubject, ErrorVerb, StorageError, Vote};
use tempfile::TempDir;

use super::{NodeId, RaftLogStoreAdapter, TypeConfig};
use crate::raft::OwnershipStateMachine;
use crate::raft::persistence::RaftLogStore;

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
async fn test_a_corrupt_persisted_value_remains_the_storage_error_source() {
    let dir = tempfile::tempdir().unwrap();
    let store = RaftLogStore::open(dir.path().join("raft.redb")).unwrap();
    store.save_vote(b"not valid json").unwrap();
    let mut adapter = RaftLogStoreAdapter::new(store);

    let error = adapter.read_vote().await.unwrap_err();

    assert!(storage_source(error).contains("expected"));
}

#[tokio::test]
async fn test_a_store_fault_remains_the_storage_error_source() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bare.redb");
    redb::Database::create(&path).unwrap();
    let mut adapter = RaftLogStoreAdapter::new(RaftLogStore::open_existing(&path).unwrap());

    let error = adapter.read_vote().await.unwrap_err();

    assert!(storage_source(error).contains("does not exist"));
}

#[rstest::rstest]
#[case::read_entries(StorageOperation::ReadEntries, ErrorSubject::Logs, ErrorVerb::Read)]
#[case::read_state(StorageOperation::ReadState, ErrorSubject::Logs, ErrorVerb::Read)]
#[case::save_vote(StorageOperation::SaveVote, ErrorSubject::Vote, ErrorVerb::Write)]
#[case::read_vote(StorageOperation::ReadVote, ErrorSubject::Vote, ErrorVerb::Read)]
#[case::append(StorageOperation::Append, ErrorSubject::Logs, ErrorVerb::Write)]
#[case::truncate(StorageOperation::Truncate, ErrorSubject::Log(log_id(1, 0, 7)), ErrorVerb::Delete)]
#[case::purge(StorageOperation::Purge, ErrorSubject::Log(log_id(1, 0, 7)), ErrorVerb::Delete)]
#[case::save_committed(StorageOperation::SaveCommitted, ErrorSubject::Logs, ErrorVerb::Write)]
#[case::read_committed(StorageOperation::ReadCommitted, ErrorSubject::Logs, ErrorVerb::Read)]
#[tokio::test]
async fn test_storage_errors_identify_the_operation(
    #[case] operation: StorageOperation,
    #[case] subject: ErrorSubject<NodeId>,
    #[case] verb: ErrorVerb,
) {
    let (_dir, mut adapter) = faulting_adapter();

    let error = operation.run(&mut adapter).await;

    assert!(
        error.to_string().starts_with(&format!("when {verb:?} {subject:?}:")),
        "{error}"
    );
}

#[derive(Clone, Copy)]
enum StorageOperation {
    ReadEntries,
    ReadState,
    SaveVote,
    ReadVote,
    Append,
    Truncate,
    Purge,
    SaveCommitted,
    ReadCommitted,
}

impl StorageOperation {
    async fn run(self, adapter: &mut RaftLogStoreAdapter) -> StorageError<NodeId> {
        match self {
            Self::ReadEntries => adapter.try_get_log_entries(..).await.unwrap_err(),
            Self::ReadState => adapter.get_log_state().await.unwrap_err(),
            Self::SaveVote => adapter.save_vote(&Vote::new(1, 0)).await.unwrap_err(),
            Self::ReadVote => adapter.read_vote().await.unwrap_err(),
            Self::Append => adapter
                .blocking_append([Entry {
                    log_id: log_id(1, 0, 7),
                    payload: EntryPayload::Blank,
                }])
                .await
                .unwrap_err(),
            Self::Truncate => adapter.truncate(log_id(1, 0, 7)).await.unwrap_err(),
            Self::Purge => adapter.purge(log_id(1, 0, 7)).await.unwrap_err(),
            Self::SaveCommitted => adapter.save_committed(Some(log_id(1, 0, 7))).await.unwrap_err(),
            Self::ReadCommitted => adapter.read_committed().await.unwrap_err(),
        }
    }
}

fn faulting_adapter() -> (TempDir, RaftLogStoreAdapter) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("incompatible.redb");
    let database = redb::Database::create(&path).unwrap();
    let transaction = database.begin_write().unwrap();
    {
        transaction
            .open_table(redb::TableDefinition::<&str, &str>::new("raft_log"))
            .unwrap();
        transaction
            .open_table(redb::TableDefinition::<u64, u64>::new("raft_meta"))
            .unwrap();
    }
    transaction.commit().unwrap();
    drop(database);
    (
        dir,
        RaftLogStoreAdapter::new(RaftLogStore::open_existing(path).unwrap()),
    )
}

fn storage_source(error: StorageError<NodeId>) -> String {
    error.into_io().unwrap().source().unwrap().to_string()
}
