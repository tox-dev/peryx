use std::io;

use openraft::storage::{IOFlushed, RaftLogReader, RaftLogStorage};
use openraft::testing::log::{StoreBuilder, Suite};
use openraft::{EntryPayload, StorageError};
use tempfile::TempDir;

use super::{Entry, LogId, LogStoreError, RaftLogStoreAdapter, TypeConfig, Vote};
use crate::raft::OwnershipStateMachine;
use crate::raft::persistence::RaftLogStore;

struct Builder;

impl StoreBuilder<TypeConfig, RaftLogStoreAdapter, OwnershipStateMachine, TempDir> for Builder {
    fn build(
        &self,
    ) -> impl Future<Output = Result<(TempDir, RaftLogStoreAdapter, OwnershipStateMachine), StorageError<TypeConfig>>> + Send
    {
        let dir = tempfile::tempdir().unwrap();
        let store = RaftLogStore::open(dir.path().join("raft.redb")).unwrap();
        std::future::ready(Ok((
            dir,
            RaftLogStoreAdapter::new(store),
            OwnershipStateMachine::default(),
        )))
    }
}

#[tokio::test]
async fn test_passes_the_openraft_storage_conformance_suite() {
    Suite::test_all(Builder).await.unwrap();
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

    assert!(error.to_string().contains("expected"), "{error}");
}

#[tokio::test]
async fn test_a_store_fault_remains_the_storage_error_source() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bare.redb");
    redb::Database::create(&path).unwrap();
    let mut adapter = RaftLogStoreAdapter::new(RaftLogStore::open_existing(&path).unwrap());

    let error = adapter.read_vote().await.unwrap_err();

    assert!(error.to_string().contains("does not exist"), "{error}");
}

#[rstest::rstest]
#[case::read_entries(StorageOperation::ReadEntries)]
#[case::read_state(StorageOperation::ReadState)]
#[case::save_vote(StorageOperation::SaveVote)]
#[case::read_vote(StorageOperation::ReadVote)]
#[case::append(StorageOperation::Append)]
#[case::truncate(StorageOperation::Truncate)]
#[case::purge(StorageOperation::Purge)]
#[case::save_committed(StorageOperation::SaveCommitted)]
#[case::read_committed(StorageOperation::ReadCommitted)]
#[tokio::test]
async fn test_every_storage_operation_surfaces_the_store_fault(#[case] operation: StorageOperation) {
    let (_dir, mut adapter) = faulting_adapter();

    let error = operation.run(&mut adapter).await;

    let source = error
        .get_ref()
        .and_then(|source| source.downcast_ref::<LogStoreError>());
    assert!(matches!(source, Some(LogStoreError::Store(_))), "{error:?}");
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
    async fn run(self, adapter: &mut RaftLogStoreAdapter) -> io::Error {
        match self {
            Self::ReadEntries => adapter.try_get_log_entries(..).await.unwrap_err(),
            Self::ReadState => adapter.get_log_state().await.unwrap_err(),
            Self::SaveVote => adapter.save_vote(&Vote::new(1, 0)).await.unwrap_err(),
            Self::ReadVote => adapter.read_vote().await.unwrap_err(),
            Self::Append => adapter
                .append(
                    [Entry {
                        log_id: log_id(1, 0, 7),
                        payload: EntryPayload::Blank,
                    }],
                    IOFlushed::noop(),
                )
                .await
                .unwrap_err(),
            Self::Truncate => adapter.truncate_after(Some(log_id(1, 0, 7))).await.unwrap_err(),
            Self::Purge => adapter.purge(log_id(1, 0, 7)).await.unwrap_err(),
            Self::SaveCommitted => adapter.save_committed(Some(log_id(1, 0, 7))).await.unwrap_err(),
            Self::ReadCommitted => adapter.read_committed().await.unwrap_err(),
        }
    }
}

fn log_id(term: u64, node_id: u64, index: u64) -> LogId {
    openraft::testing::log_id::<TypeConfig>(term, node_id, index)
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
