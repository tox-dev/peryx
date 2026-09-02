use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use openraft::storage::RaftStateMachine;
use openraft::testing::log_id;
use openraft::{Entry, EntryPayload, Membership, RaftSnapshotBuilder, Snapshot, SnapshotMeta};
use redb::backends::InMemoryBackend;
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::ownership::{Assignment, AssignmentCause, DatacenterId, OwnershipCommand, OwnershipEffect, OwnershipState};
use crate::raft::persistence::RaftLogStore;
use crate::raft::{OwnershipResponse, OwnershipStateMachine, PeryxNode, TypeConfig};
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
        now_unix: 0,
    }
}

fn move_home(authority: &str, new_home: &str) -> OwnershipCommand {
    OwnershipCommand::AttemptControl {
        key: "k1".to_owned(),
        command: peryx_ha::ControlCommand::TransferAuthority {
            authority: authority.to_owned(),
            new_home: new_home.to_owned(),
            intent: Some(peryx_ha::TransferIntent {
                source: "east".to_owned(),
                actor: "alice".to_owned(),
                reason: "drain east".to_owned(),
                barrier: 5,
            }),
        },
        now_unix: 0,
    }
}

fn advance_control(authority: &str) -> OwnershipCommand {
    OwnershipCommand::AttemptControl {
        key: "k1".to_owned(),
        command: peryx_ha::ControlCommand::AdvanceEpoch {
            authority: authority.to_owned(),
        },
        now_unix: 0,
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
        payload: EntryPayload::Membership(roster_membership()),
    }
}

fn roster_membership() -> Membership<u64, PeryxNode> {
    Membership::new(
        vec![BTreeSet::from([0])],
        BTreeMap::from([(
            0,
            PeryxNode {
                datacenter: DatacenterId("east".to_owned()),
                endpoint: "http://east.internal:4460/".to_owned(),
            },
        )]),
    )
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
                home: DatacenterId("east".to_owned()),
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
    assert_eq!(membership.membership(), &roster_membership());
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

async fn snapshot_without_projector_acknowledgements() -> (SnapshotMeta<u64, PeryxNode>, Vec<u8>) {
    let mut source = OwnershipStateMachine::default();
    source
        .apply(vec![
            membership_entry(1),
            normal(2, assign("proj", "east")),
            normal(3, move_home("proj", "west")),
        ])
        .await
        .unwrap();
    let Snapshot { meta, snapshot } = source.get_snapshot_builder().await.build_snapshot().await.unwrap();
    let mut state: serde_json::Value = serde_json::from_slice(&snapshot.into_inner()).unwrap();
    state.as_object_mut().unwrap().remove("audit_projectors");
    for record in state["controls"].as_object_mut().unwrap().values_mut() {
        let record = record.as_object_mut().unwrap();
        record.remove("audit_projectors");
        record["receipt"].as_object_mut().unwrap().remove("transfer_audit");
    }
    (meta, serde_json::to_vec(&state).unwrap())
}

#[tokio::test]
async fn test_installing_an_older_snapshot_restores_the_audit_in_its_receipt() {
    let (meta, snapshot) = snapshot_without_projector_acknowledgements().await;
    let mut restored = OwnershipStateMachine::default();

    restored
        .install_snapshot(&meta, Box::new(std::io::Cursor::new(snapshot)))
        .await
        .unwrap();
    let response = restored
        .apply(vec![normal(4, move_home("proj", "west"))])
        .await
        .unwrap();

    assert!(matches!(
        &response[0],
        OwnershipResponse::Applied(OwnershipEffect::Control(crate::ControlResolution::Replayed(
            peryx_ha::CommandReceipt {
                transfer_audit: Some(_),
                ..
            }
        )))
    ));
}

#[tokio::test]
async fn test_installing_a_snapshot_keeps_a_non_transfer_receipt_unaudited() {
    let mut source = OwnershipStateMachine::default();
    source
        .apply(vec![
            membership_entry(1),
            normal(2, assign("proj", "east")),
            normal(3, advance_control("proj")),
        ])
        .await
        .unwrap();
    let Snapshot { meta, snapshot } = source.get_snapshot_builder().await.build_snapshot().await.unwrap();
    let mut restored = OwnershipStateMachine::default();
    restored.install_snapshot(&meta, snapshot).await.unwrap();

    let response = restored.apply(vec![normal(4, advance_control("proj"))]).await.unwrap();

    assert!(matches!(
        &response[0],
        OwnershipResponse::Applied(OwnershipEffect::Control(crate::ControlResolution::Replayed(
            peryx_ha::CommandReceipt {
                outcome: peryx_ha::CommandOutcome::Committed,
                transfer_audit: None,
                ..
            }
        )))
    ));
    assert_eq!(restored.pending_transfer_audits("east").await, Vec::new());
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

#[tokio::test]
async fn test_pending_transfer_audits_reads_the_facts_committed_transfers_sealed() {
    let mut machine = OwnershipStateMachine::default();
    machine
        .apply(vec![membership_entry(1), normal(2, assign("proj", "east"))])
        .await
        .unwrap();
    assert_eq!(machine.pending_transfer_audits("east").await, Vec::new());

    machine.apply(vec![normal(3, move_home("proj", "west"))]).await.unwrap();

    assert_eq!(
        machine.pending_transfer_audits("east").await,
        vec![peryx_ha::PendingTransferAudit {
            id: "k1".to_owned(),
            audit: peryx_ha::TransferAudit {
                authority: "proj".to_owned(),
                source: "east".to_owned(),
                target: "west".to_owned(),
                actor: "alice".to_owned(),
                reason: "drain east".to_owned(),
                barrier: 5,
                epoch: 2,
                commit_term: 1,
                commit_index: 3,
            },
        }]
    );
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

    let mut restored = OwnershipStateMachine::with_snapshot_store(reopen_store(&dir)).unwrap();
    assert_eq!(
        restored.home_of(&key("proj")).await,
        Some(DatacenterId("east".to_owned()))
    );
    assert_eq!(restored.epoch_of(&key("proj")).await, AuthorityEpoch(2));
    let (last_applied, membership) = restored.applied_state().await.unwrap();
    assert_eq!(last_applied, Some(log_id(1, 0, 3)));
    assert_eq!(membership.log_id(), &Some(log_id(1, 0, 2)));
    assert_eq!(membership.membership(), &roster_membership());
}

#[tokio::test]
async fn test_reopening_an_older_snapshot_restores_pending_projection_members() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(&dir);
    let (meta, snapshot) = snapshot_without_projector_acknowledgements().await;
    store
        .save_snapshot(&serde_json::to_vec(&meta).unwrap(), &snapshot, 1)
        .unwrap();

    let restored = OwnershipStateMachine::with_snapshot_store(store).unwrap();

    assert_eq!(restored.pending_transfer_audits("east").await.len(), 1);
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

/// A build counter that lives only in the process restarts at zero, so the replacement re-issues an
/// identifier the previous process already handed out at the same applied index.
#[tokio::test]
async fn test_rebuilding_after_a_restart_takes_a_new_snapshot_id_at_the_same_log_index() {
    let dir = tempfile::tempdir().unwrap();
    let mut machine = OwnershipStateMachine::with_snapshot_store(open_store(&dir)).unwrap();
    machine.apply(vec![normal(1, assign("proj", "east"))]).await.unwrap();
    let before = machine
        .get_snapshot_builder()
        .await
        .build_snapshot()
        .await
        .unwrap()
        .meta;
    drop(machine);

    let mut restarted = OwnershipStateMachine::with_snapshot_store(reopen_store(&dir)).unwrap();
    let after = restarted
        .get_snapshot_builder()
        .await
        .build_snapshot()
        .await
        .unwrap()
        .meta;

    assert_eq!(before.snapshot_id, "1-1");
    assert_eq!(after.snapshot_id, "1-2");
    assert_eq!(after.last_log_id, before.last_log_id);
}

/// Uniqueness must not cost lookup: the identifier a restart issues is the one both the running
/// machine and the reopened store answer with.
#[tokio::test]
async fn test_a_snapshot_id_taken_after_a_restart_still_resolves_to_its_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let mut machine = OwnershipStateMachine::with_snapshot_store(open_store(&dir)).unwrap();
    machine.apply(vec![normal(1, assign("proj", "east"))]).await.unwrap();
    machine.get_snapshot_builder().await.build_snapshot().await.unwrap();
    drop(machine);
    let mut restarted = OwnershipStateMachine::with_snapshot_store(reopen_store(&dir)).unwrap();
    let rebuilt = restarted.get_snapshot_builder().await.build_snapshot().await.unwrap();

    let current = restarted.get_current_snapshot().await.unwrap().unwrap();
    drop(restarted);
    let mut reopened = OwnershipStateMachine::with_snapshot_store(reopen_store(&dir)).unwrap();
    let persisted = reopened.get_current_snapshot().await.unwrap().unwrap();

    assert_eq!(current.meta, rebuilt.meta);
    assert_eq!(persisted.meta, rebuilt.meta);
    assert_eq!(persisted.snapshot.into_inner(), rebuilt.snapshot.into_inner());
}

/// Each restart advances the generation rather than resuming it, so no two processes over one store
/// name a snapshot alike.
#[tokio::test]
async fn test_repeated_restarts_never_reissue_an_earlier_snapshot_id() {
    let dir = tempfile::tempdir().unwrap();
    let mut seeded = OwnershipStateMachine::with_snapshot_store(open_store(&dir)).unwrap();
    seeded.apply(vec![normal(9, assign("proj", "east"))]).await.unwrap();
    seeded.get_snapshot_builder().await.build_snapshot().await.unwrap();
    drop(seeded);

    let mut ids = BTreeSet::new();
    for _ in 0..2 {
        let mut restarted = OwnershipStateMachine::with_snapshot_store(reopen_store(&dir)).unwrap();
        let built = restarted.get_snapshot_builder().await.build_snapshot().await.unwrap();
        ids.insert(built.meta.snapshot_id);
    }

    assert_eq!(ids, BTreeSet::from(["9-2".to_owned(), "9-3".to_owned()]));
}

/// Installing a peer's snapshot adopts that peer's identifier, and must leave this store's own
/// generation standing, or the next local build would republish the identifier just installed.
#[tokio::test]
async fn test_installing_a_snapshot_leaves_the_local_generation_standing() {
    let mut source = OwnershipStateMachine::default();
    source.apply(vec![normal(1, assign("proj", "east"))]).await.unwrap();
    let Snapshot { meta, snapshot } = source.get_snapshot_builder().await.build_snapshot().await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let mut target = OwnershipStateMachine::with_snapshot_store(open_store(&dir)).unwrap();
    target.get_snapshot_builder().await.build_snapshot().await.unwrap();
    target.install_snapshot(&meta, snapshot).await.unwrap();
    drop(target);

    let mut restarted = OwnershipStateMachine::with_snapshot_store(reopen_store(&dir)).unwrap();
    let rebuilt = restarted.get_snapshot_builder().await.build_snapshot().await.unwrap();

    assert_eq!(meta.snapshot_id, "1-1");
    assert_eq!(rebuilt.meta.snapshot_id, "1-2");
    assert_eq!(
        restarted.home_of(&key("proj")).await,
        Some(DatacenterId("east".to_owned()))
    );
}

#[tokio::test]
async fn test_with_snapshot_store_surfaces_a_store_read_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bare.redb");
    redb::Database::create(&path).unwrap();

    let error = OwnershipStateMachine::with_snapshot_store(RaftLogStore::open_existing(&path).unwrap()).unwrap_err();

    assert!(error.to_string().to_lowercase().contains("does not exist"), "{error}");
}

#[tokio::test]
async fn test_with_snapshot_store_surfaces_a_corrupt_snapshot_meta() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(&dir);
    store.save_snapshot(b"not valid json", b"[]", 1).unwrap();

    let error = OwnershipStateMachine::with_snapshot_store(store).unwrap_err();

    assert!(error.to_string().contains("expected"), "{error}");
}

#[tokio::test]
async fn test_with_snapshot_store_rejects_a_snapshot_whose_state_is_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(&dir);
    let mut machine = OwnershipStateMachine::with_snapshot_store(store.clone()).unwrap();
    machine.apply(vec![normal(1, assign("proj", "east"))]).await.unwrap();
    machine.get_snapshot_builder().await.build_snapshot().await.unwrap();
    let stored = store.read_snapshot().unwrap().unwrap();
    store.save_snapshot(&stored.meta, b"not a snapshot", 1).unwrap();
    drop(machine);

    let error = OwnershipStateMachine::with_snapshot_store(store).unwrap_err();

    assert!(error.to_string().contains("expected"), "{error}");
}

#[tokio::test]
async fn test_admit_fences_a_superseded_epoch() {
    let mut machine = OwnershipStateMachine::default();
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

/// Bounds a failure only. Each awaited step returns as soon as the gate reports the commit parked.
const GATE_TIMEOUT: Duration = Duration::from_secs(10);

/// A redb backend that parks one commit inside `sync_data` until the test releases it, so a test can
/// drive the state machine while a snapshot write is genuinely in flight.
#[derive(Debug)]
struct GatedBackend {
    inner: InMemoryBackend,
    armed: Arc<AtomicBool>,
    entered: mpsc::UnboundedSender<()>,
    release: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl redb::StorageBackend for GatedBackend {
    fn len(&self) -> std::io::Result<u64> {
        self.inner.len()
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> std::io::Result<()> {
        self.inner.read(offset, out)
    }

    fn set_len(&self, len: u64) -> std::io::Result<()> {
        self.inner.set_len(len)
    }

    fn sync_data(&self) -> std::io::Result<()> {
        if self.armed.swap(false, Ordering::SeqCst) {
            let release = self.release.lock().unwrap().take().unwrap();
            self.entered.send(()).unwrap();
            release.recv().unwrap();
        }
        self.inner.sync_data()
    }

    fn write(&self, offset: u64, data: &[u8]) -> std::io::Result<()> {
        self.inner.write(offset, data)
    }
}

struct Gate {
    armed: Arc<AtomicBool>,
    entered: mpsc::UnboundedReceiver<()>,
    release: std::sync::mpsc::Sender<()>,
}

impl Gate {
    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    /// Returns once a commit has reached the backend and parked there.
    async fn await_parked_commit(&mut self) {
        self.entered.recv().await.unwrap();
    }

    fn release(&self) {
        self.release.send(()).unwrap();
    }
}

impl Drop for Gate {
    /// Unparks a commit the test left waiting, so a failed assertion reports itself instead of
    /// wedging the runtime shutdown behind a blocking thread that never returns.
    fn drop(&mut self) {
        let _ = self.release.send(());
    }
}

fn gated_store() -> (RaftLogStore, Gate) {
    let armed = Arc::new(AtomicBool::new(false));
    let (entered, entered_receiver) = mpsc::unbounded_channel();
    let (release, release_receiver) = std::sync::mpsc::channel();
    let store = RaftLogStore::open_backend(GatedBackend {
        inner: InMemoryBackend::new(),
        armed: armed.clone(),
        entered,
        release: std::sync::Mutex::new(Some(release_receiver)),
    })
    .unwrap();
    (
        store,
        Gate {
            armed,
            entered: entered_receiver,
            release,
        },
    )
}

/// A fault poisons the database handle it hit, so reading the pages back needs a fresh one.
fn reopen_pages(pages: &Arc<InMemoryBackend>, fault: &Arc<peryx_test_support::fault::Fault>) -> RaftLogStore {
    RaftLogStore::reopen_backend(peryx_test_support::fault::faulted(pages, fault)).unwrap()
}

async fn snapshot_of(entries: Vec<Entry<TypeConfig>>) -> (SnapshotMeta<u64, PeryxNode>, Vec<u8>) {
    let mut source = OwnershipStateMachine::default();
    source.apply(entries).await.unwrap();
    let Snapshot { meta, snapshot } = source.get_snapshot_builder().await.build_snapshot().await.unwrap();
    (meta, snapshot.into_inner())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_a_parked_build_commit_holds_up_neither_a_read_nor_an_apply() {
    let (store, mut gate) = gated_store();
    let mut machine = OwnershipStateMachine::with_snapshot_store(store).unwrap();
    machine.apply(vec![normal(1, assign("proj", "east"))]).await.unwrap();
    let mut builder = machine.get_snapshot_builder().await;

    gate.arm();
    let build = tokio::spawn(async move { builder.build_snapshot().await });
    gate.await_parked_commit().await;

    let epoch = timeout(GATE_TIMEOUT, machine.epoch_of(&key("proj"))).await;
    let responses = timeout(GATE_TIMEOUT, machine.apply(vec![normal(2, advance("proj"))])).await;
    gate.release();
    let Snapshot { snapshot, .. } = build.await.unwrap().unwrap();

    let epoch = epoch.expect("read the applied epoch while the commit is parked");
    let responses = responses.expect("apply an entry while the commit is parked").unwrap();
    assert_eq!(epoch, AuthorityEpoch(1));
    assert_eq!(
        responses,
        vec![OwnershipResponse::Applied(OwnershipEffect::EpochAdvanced {
            epoch: AuthorityEpoch(2)
        })]
    );
    // The build captured the state it cloned, so the later apply did not leak into it.
    let captured = OwnershipState::restore(&snapshot.into_inner()).unwrap();
    assert_eq!(captured.epoch(&key("proj")), AuthorityEpoch(1));
    assert_eq!(machine.epoch_of(&key("proj")).await, AuthorityEpoch(2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_a_parked_install_commit_shows_the_state_the_node_had_before_it() {
    let (meta, data) = snapshot_of(vec![normal(1, assign("proj", "west")), normal(2, advance("proj"))]).await;
    let (store, mut gate) = gated_store();
    let mut machine = OwnershipStateMachine::with_snapshot_store(store).unwrap();
    machine.apply(vec![normal(1, assign("proj", "east"))]).await.unwrap();
    let mut installer = machine.clone();

    gate.arm();
    let install = tokio::spawn(async move { installer.install_snapshot(&meta, Box::new(Cursor::new(data))).await });
    gate.await_parked_commit().await;

    let during = timeout(GATE_TIMEOUT, machine.home_claim(&key("proj"))).await;
    gate.release();
    install.await.unwrap().unwrap();
    let during = during.expect("read the applied claim while the install commit is parked");

    // No half-installed state is observable: the reader saw the node's own assignment, and the
    // restored home and epoch appear together once the install publishes.
    assert_eq!(during, Some((DatacenterId("east".to_owned()), AuthorityEpoch(1))));
    assert_eq!(
        machine.home_claim(&key("proj")).await,
        Some((DatacenterId("west".to_owned()), AuthorityEpoch(2))),
    );
}

#[tokio::test]
async fn test_a_late_candidate_replaces_neither_the_durable_nor_the_current_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(&dir);
    let mut machine = OwnershipStateMachine::with_snapshot_store(store.clone()).unwrap();
    machine.apply(vec![normal(1, assign("proj", "east"))]).await.unwrap();
    let older = machine.snapshot_candidate().await;
    machine.apply(vec![normal(2, advance("proj"))]).await.unwrap();
    let newer = machine.snapshot_candidate().await;

    machine.store_candidate(newer).await.unwrap();
    machine.store_candidate(older).await.unwrap();

    let durable = OwnershipState::restore(&store.read_snapshot().unwrap().unwrap().data).unwrap();
    assert_eq!(durable.epoch(&key("proj")), AuthorityEpoch(2));
    let current = machine.get_current_snapshot().await.unwrap().unwrap();
    let published = OwnershipState::restore(&current.snapshot.into_inner()).unwrap();
    assert_eq!(published.epoch(&key("proj")), AuthorityEpoch(2));
}

#[tokio::test]
async fn test_a_failed_build_commit_leaves_the_previous_snapshot_standing() {
    let (pages, fault) = peryx_test_support::fault::backend();
    let store = RaftLogStore::open_backend(peryx_test_support::fault::faulted(&pages, &fault)).unwrap();
    let mut machine = OwnershipStateMachine::with_snapshot_store(store).unwrap();
    machine.apply(vec![normal(1, assign("proj", "east"))]).await.unwrap();
    machine.get_snapshot_builder().await.build_snapshot().await.unwrap();
    machine.apply(vec![normal(2, advance("proj"))]).await.unwrap();

    fault.arm(0);
    let error = machine.get_snapshot_builder().await.build_snapshot().await.unwrap_err();
    fault.disable();

    assert!(error.to_string().contains("injected storage failure"), "{error}");
    let durable =
        OwnershipState::restore(&reopen_pages(&pages, &fault).read_snapshot().unwrap().unwrap().data).unwrap();
    assert_eq!(durable.epoch(&key("proj")), AuthorityEpoch(1));
    let current = machine.get_current_snapshot().await.unwrap().unwrap();
    let published = OwnershipState::restore(&current.snapshot.into_inner()).unwrap();
    assert_eq!(published.epoch(&key("proj")), AuthorityEpoch(1));
}

#[tokio::test]
async fn test_a_failed_install_commit_leaves_the_previous_snapshot_standing() {
    let (meta, data) = snapshot_of(vec![normal(1, assign("proj", "west")), normal(2, advance("proj"))]).await;
    let (pages, fault) = peryx_test_support::fault::backend();
    let store = RaftLogStore::open_backend(peryx_test_support::fault::faulted(&pages, &fault)).unwrap();
    let mut machine = OwnershipStateMachine::with_snapshot_store(store).unwrap();
    machine.apply(vec![normal(1, assign("proj", "east"))]).await.unwrap();
    machine.get_snapshot_builder().await.build_snapshot().await.unwrap();

    fault.arm(0);
    let error = machine
        .install_snapshot(&meta, Box::new(Cursor::new(data)))
        .await
        .unwrap_err();
    fault.disable();

    assert!(error.to_string().contains("injected storage failure"), "{error}");
    let durable =
        OwnershipState::restore(&reopen_pages(&pages, &fault).read_snapshot().unwrap().unwrap().data).unwrap();
    assert_eq!(durable.epoch(&key("proj")), AuthorityEpoch(1));
    assert_eq!(
        machine.home_of(&key("proj")).await,
        Some(DatacenterId("east".to_owned()))
    );
}
