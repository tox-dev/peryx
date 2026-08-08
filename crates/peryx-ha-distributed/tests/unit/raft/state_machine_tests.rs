use std::collections::BTreeSet;

use openraft::storage::RaftStateMachine;
use openraft::testing::log_id;
use openraft::{Entry, EntryPayload, Membership, RaftSnapshotBuilder, Snapshot};
use peryx_storage::raft::RaftLogStore;
use tempfile::TempDir;

use crate::ownership::{Assignment, AssignmentCause, DatacenterId, OwnershipCommand, OwnershipEffect, OwnershipState};
use crate::raft::{OwnershipResponse, OwnershipStateMachine, TypeConfig};
use crate::{Admission, AuthorityEpoch};

fn key(name: &str) -> crate::AuthorityKey {
    crate::AuthorityKey(name.to_owned())
}

fn assign(authority: &str, home: &str) -> OwnershipCommand {
    OwnershipCommand::AssignHome {
        authority: key(authority),
        home: DatacenterId(home.to_owned()),
        cause: AssignmentCause::FirstPublish,
    }
}

fn advance(authority: &str) -> OwnershipCommand {
    OwnershipCommand::AdvanceAuthorityEpoch {
        authority: key(authority),
    }
}

fn normal(index: u64, command: OwnershipCommand) -> Entry<TypeConfig> {
    normal_at(1, index, command)
}

fn normal_at(term: u64, index: u64, command: OwnershipCommand) -> Entry<TypeConfig> {
    Entry {
        log_id: log_id(term, 0, index),
        payload: EntryPayload::Normal(command),
    }
}

fn blank(index: u64) -> Entry<TypeConfig> {
    Entry {
        log_id: log_id(1, 0, index),
        payload: EntryPayload::Blank,
    }
}

fn membership_entry(index: u64) -> Entry<TypeConfig> {
    Entry {
        log_id: log_id(1, 0, index),
        payload: EntryPayload::Membership(Membership::new(vec![BTreeSet::from([0])], ())),
    }
}

#[tokio::test]
async fn test_apply_folds_normal_commands_through_one_state_in_order() {
    let mut machine = OwnershipStateMachine::default();

    let responses = machine
        .apply(vec![normal(1, assign("proj", "east")), normal(2, advance("proj"))])
        .await
        .unwrap();

    assert_eq!(
        responses,
        vec![
            OwnershipResponse::Applied(OwnershipEffect::Assigned {
                epoch: AuthorityEpoch(1)
            }),
            OwnershipResponse::Applied(OwnershipEffect::EpochAdvanced {
                epoch: AuthorityEpoch(2)
            }),
        ]
    );
}

#[tokio::test]
async fn test_apply_stamps_the_committed_term_and_index_onto_the_assignment_audit() {
    let mut machine = OwnershipStateMachine::default();

    machine
        .apply(vec![normal_at(4, 9, assign("proj", "east"))])
        .await
        .unwrap();

    // The apply loop reads the entry's log position, so the assignment audit - read back through the
    // snapshot the machine builds - carries the term and index the command committed at.
    let snapshot = machine.get_snapshot_builder().await.build_snapshot().await.unwrap();
    let restored = OwnershipState::restore(&snapshot.snapshot.into_inner()).unwrap();
    assert_eq!(
        restored.assignment(&key("proj")),
        Some(&Assignment {
            cause: AssignmentCause::FirstPublish,
            term: 4,
            index: 9,
            epoch: AuthorityEpoch(1),
        })
    );
}

#[tokio::test]
async fn test_apply_records_the_last_applied_log_id() {
    let mut machine = OwnershipStateMachine::default();

    machine.apply(vec![normal(1, assign("proj", "east"))]).await.unwrap();

    let (last_applied, _membership) = machine.applied_state().await.unwrap();
    assert_eq!(last_applied, Some(log_id(1, 0, 1)));
}

#[tokio::test]
async fn test_a_blank_entry_applies_as_non_mutating() {
    let mut machine = OwnershipStateMachine::default();

    let responses = machine.apply(vec![blank(1)]).await.unwrap();

    assert_eq!(responses, vec![OwnershipResponse::NonMutating]);
    let (last_applied, _) = machine.applied_state().await.unwrap();
    assert_eq!(last_applied, Some(log_id(1, 0, 1)));
}

#[tokio::test]
async fn test_a_membership_entry_records_membership_without_mutating_ownership() {
    let mut machine = OwnershipStateMachine::default();

    let responses = machine.apply(vec![membership_entry(4)]).await.unwrap();

    assert_eq!(responses, vec![OwnershipResponse::NonMutating]);
    let (_, membership) = machine.applied_state().await.unwrap();
    assert_eq!(membership.log_id(), &Some(log_id(1, 0, 4)));
    assert_eq!(membership.membership(), &Membership::new(vec![BTreeSet::from([0])], ()));
}

#[tokio::test]
async fn test_applied_state_starts_empty() {
    let mut machine = OwnershipStateMachine::default();

    let (last_applied, membership) = machine.applied_state().await.unwrap();

    assert_eq!(last_applied, None);
    assert_eq!(membership.log_id(), &None);
}

#[tokio::test]
async fn test_get_current_snapshot_is_none_before_any_build() {
    let mut machine = OwnershipStateMachine::default();

    assert!(machine.get_current_snapshot().await.unwrap().is_none());
}

#[tokio::test]
async fn test_build_snapshot_on_an_empty_machine_carries_no_log_id() {
    let mut machine = OwnershipStateMachine::default();

    let snapshot = machine.get_snapshot_builder().await.build_snapshot().await.unwrap();

    assert_eq!(snapshot.meta.last_log_id, None);
    assert_eq!(snapshot.meta.snapshot_id, "0-1");
}

#[tokio::test]
async fn test_build_snapshot_captures_the_applied_log_id_and_is_retrievable() {
    let mut machine = OwnershipStateMachine::default();
    machine.apply(vec![normal(7, assign("proj", "east"))]).await.unwrap();

    let built = machine.get_snapshot_builder().await.build_snapshot().await.unwrap();

    assert_eq!(built.meta.last_log_id, Some(log_id(1, 0, 7)));
    let current = machine.get_current_snapshot().await.unwrap().unwrap();
    assert_eq!(current.meta, built.meta);
}

#[tokio::test]
async fn test_begin_receiving_snapshot_hands_back_an_empty_buffer() {
    let mut machine = OwnershipStateMachine::default();

    let buffer = machine.begin_receiving_snapshot().await.unwrap();

    assert!(buffer.into_inner().is_empty());
}

#[tokio::test]
async fn test_installing_a_snapshot_restores_state_onto_a_fresh_machine() {
    let mut source = OwnershipStateMachine::default();
    source
        .apply(vec![normal(1, assign("proj", "east")), normal(2, advance("proj"))])
        .await
        .unwrap();
    let Snapshot { meta, snapshot } = source.get_snapshot_builder().await.build_snapshot().await.unwrap();

    let mut restored = OwnershipStateMachine::default();
    restored.install_snapshot(&meta, snapshot).await.unwrap();

    let (last_applied, _) = restored.applied_state().await.unwrap();
    assert_eq!(last_applied, Some(log_id(1, 0, 2)));
    // The restored state carries the assigned authority: advancing it lands epoch three, which only
    // holds if the assign and the first advance survived the snapshot round-trip.
    let responses = restored.apply(vec![normal(3, advance("proj"))]).await.unwrap();
    assert_eq!(
        responses,
        vec![OwnershipResponse::Applied(OwnershipEffect::EpochAdvanced {
            epoch: AuthorityEpoch(3)
        })]
    );
}

#[tokio::test]
async fn test_installing_a_snapshot_makes_it_the_current_snapshot() {
    let mut source = OwnershipStateMachine::default();
    source.apply(vec![normal(1, assign("proj", "east"))]).await.unwrap();
    let Snapshot { meta, snapshot } = source.get_snapshot_builder().await.build_snapshot().await.unwrap();

    let mut restored = OwnershipStateMachine::default();
    restored.install_snapshot(&meta, snapshot).await.unwrap();

    assert_eq!(restored.get_current_snapshot().await.unwrap().unwrap().meta, meta);
}

#[tokio::test]
async fn test_installing_a_corrupt_snapshot_fails_closed() {
    let mut source = OwnershipStateMachine::default();
    let Snapshot { meta, .. } = source.get_snapshot_builder().await.build_snapshot().await.unwrap();

    let mut machine = OwnershipStateMachine::default();
    let result = machine
        .install_snapshot(&meta, Box::new(std::io::Cursor::new(b"not a snapshot".to_vec())))
        .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn test_home_of_reads_the_applied_home() {
    let mut machine = OwnershipStateMachine::default();
    assert_eq!(machine.home_of(&key("proj")).await, None);

    machine.apply(vec![normal(1, assign("proj", "east"))]).await.unwrap();

    assert_eq!(
        machine.home_of(&key("proj")).await,
        Some(DatacenterId("east".to_owned()))
    );
}

#[tokio::test]
async fn test_epoch_of_reads_the_committed_epoch() {
    let mut machine = OwnershipStateMachine::default();
    assert_eq!(machine.epoch_of(&key("proj")).await, AuthorityEpoch(0));

    machine.apply(vec![normal(1, assign("proj", "east"))]).await.unwrap();
    assert_eq!(machine.epoch_of(&key("proj")).await, AuthorityEpoch(1));

    machine.apply(vec![normal(2, advance("proj"))]).await.unwrap();
    assert_eq!(machine.epoch_of(&key("proj")).await, AuthorityEpoch(2));
}

fn open_store(dir: &TempDir) -> RaftLogStore {
    RaftLogStore::open(dir.path().join("raft.redb")).unwrap()
}

fn reopen_store(dir: &TempDir) -> RaftLogStore {
    RaftLogStore::open_existing(dir.path().join("raft.redb")).unwrap()
}

#[tokio::test]
async fn test_with_snapshot_store_on_a_fresh_store_starts_empty() {
    let dir = tempfile::tempdir().unwrap();
    let mut machine = OwnershipStateMachine::with_snapshot_store(open_store(&dir)).unwrap();

    assert!(machine.get_current_snapshot().await.unwrap().is_none());
    let (last_applied, membership) = machine.applied_state().await.unwrap();
    assert_eq!(last_applied, None);
    assert_eq!(membership.log_id(), &None);
}

#[tokio::test]
async fn test_a_built_snapshot_survives_reopening_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let mut machine = OwnershipStateMachine::with_snapshot_store(open_store(&dir)).unwrap();
    machine
        .apply(vec![
            normal(1, assign("proj", "east")),
            membership_entry(2),
            normal(3, advance("proj")),
        ])
        .await
        .unwrap();
    machine.get_snapshot_builder().await.build_snapshot().await.unwrap();
    drop(machine);

    // Reopening the store rebuilds the machine from the persisted snapshot alone - it never replays the
    // log - so ownership, epochs, membership, and last_applied must all survive the restart.
    let mut restored = OwnershipStateMachine::with_snapshot_store(reopen_store(&dir)).unwrap();
    assert_eq!(
        restored.home_of(&key("proj")).await,
        Some(DatacenterId("east".to_owned()))
    );
    assert_eq!(restored.epoch_of(&key("proj")).await, AuthorityEpoch(2));
    let (last_applied, membership) = restored.applied_state().await.unwrap();
    assert_eq!(last_applied, Some(log_id(1, 0, 3)));
    assert_eq!(membership.log_id(), &Some(log_id(1, 0, 2)));
    assert_eq!(membership.membership(), &Membership::new(vec![BTreeSet::from([0])], ()));
}

#[tokio::test]
async fn test_an_installed_snapshot_survives_reopening_the_store() {
    let mut source = OwnershipStateMachine::default();
    source.apply(vec![normal(1, assign("proj", "east"))]).await.unwrap();
    let Snapshot { meta, snapshot } = source.get_snapshot_builder().await.build_snapshot().await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let mut target = OwnershipStateMachine::with_snapshot_store(open_store(&dir)).unwrap();
    target.install_snapshot(&meta, snapshot).await.unwrap();
    drop(target);

    let mut restored = OwnershipStateMachine::with_snapshot_store(reopen_store(&dir)).unwrap();
    assert_eq!(
        restored.home_of(&key("proj")).await,
        Some(DatacenterId("east".to_owned()))
    );
    let (last_applied, _) = restored.applied_state().await.unwrap();
    assert_eq!(last_applied, Some(log_id(1, 0, 1)));
}

#[tokio::test]
async fn test_with_snapshot_store_surfaces_a_store_read_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bare.redb");
    // A valid redb database without the raft tables: reading the persisted snapshot hits redb's "table
    // does not exist" fault, which folds through the store-error arm into a storage error.
    redb::Database::create(&path).unwrap();

    let error = OwnershipStateMachine::with_snapshot_store(RaftLogStore::open_existing(&path).unwrap()).unwrap_err();

    assert!(error.to_string().to_lowercase().contains("does not exist"), "{error}");
}

#[tokio::test]
async fn test_with_snapshot_store_surfaces_a_corrupt_snapshot_meta() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(&dir);
    // Metadata bytes the snapshot-meta decoder cannot parse, written under the store's own snapshot keys.
    store.save_snapshot(b"not valid json", b"[]").unwrap();

    let error = OwnershipStateMachine::with_snapshot_store(store).unwrap_err();

    assert!(error.to_string().contains("expected"), "{error}");
}

#[tokio::test]
async fn test_with_snapshot_store_rejects_a_snapshot_whose_state_is_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(&dir);
    // Persist a real snapshot, then overwrite its data with bytes the ownership state cannot restore while
    // keeping the valid metadata, so the restore - not the meta decode - is what fails.
    let mut machine = OwnershipStateMachine::with_snapshot_store(store.clone()).unwrap();
    machine.apply(vec![normal(1, assign("proj", "east"))]).await.unwrap();
    machine.get_snapshot_builder().await.build_snapshot().await.unwrap();
    let stored = store.read_snapshot().unwrap().unwrap();
    store.save_snapshot(&stored.meta, b"not a snapshot").unwrap();
    drop(machine);

    let error = OwnershipStateMachine::with_snapshot_store(store).unwrap_err();

    assert!(error.to_string().contains("expected"), "{error}");
}

#[tokio::test]
async fn test_admit_fences_a_superseded_epoch() {
    let mut machine = OwnershipStateMachine::default();
    // An unassigned authority admits nothing, so its zero epoch fences even a real presented epoch.
    assert_eq!(
        machine.admit(&key("proj"), AuthorityEpoch(1)).await,
        Admission::Fenced {
            committed: AuthorityEpoch(0),
            presented: AuthorityEpoch(1),
        }
    );

    machine.apply(vec![normal(1, assign("proj", "east"))]).await.unwrap();
    assert_eq!(machine.admit(&key("proj"), AuthorityEpoch(1)).await, Admission::Admit);
    assert_eq!(
        machine.admit(&key("proj"), AuthorityEpoch(2)).await,
        Admission::Fenced {
            committed: AuthorityEpoch(1),
            presented: AuthorityEpoch(2),
        }
    );
}
