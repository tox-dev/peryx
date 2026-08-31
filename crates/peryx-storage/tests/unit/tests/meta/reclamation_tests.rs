use peryx_ha::{
    BackendId, BackendLocation, BlobPlacementKey, BlobPlacementRecord, BlobPlacementState, CompareWrite, DataCenterId,
    ReclamationSnapshot, ReclamationState, ReclamationStore as _, ReclamationTombstone, TombstoneWrite,
};
use peryx_identity::ArtifactDigest;
use tempfile::TempDir;

use crate::meta::MetaStore;

fn store() -> (TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    store.initialize_distributed_state().unwrap();
    (dir, store)
}

fn digest(suffix: u8) -> ArtifactDigest {
    ArtifactDigest::from_sha256(format!("{suffix:064x}")).unwrap()
}

fn tombstone(suffix: u8, fence: u64) -> ReclamationTombstone {
    ReclamationTombstone {
        digest: digest(suffix),
        state: ReclamationState::Pending,
        required_frontier: 7,
        fence,
        attempts: 1,
        selected_at_unix: 10,
        updated_at_unix: 10,
    }
}

fn placement(suffix: u8) -> BlobPlacementRecord {
    BlobPlacementRecord {
        key: BlobPlacementKey {
            digest: digest(suffix),
            backend: BackendId::new("filesystem").unwrap(),
            data_center: DataCenterId::new("home").unwrap(),
            location: BackendLocation::new("blobs/aa").unwrap(),
        },
        state: BlobPlacementState::Pending,
        fence: 1,
        transfer_attempt: 1,
        generation: 1,
        updated_at_unix: 10,
    }
}

fn write_tombstone(store: &MetaStore, record: &ReclamationTombstone) {
    let expected = store.reclamation_snapshot(&record.digest).unwrap();
    let references = store.reference_revision().unwrap();
    assert_eq!(
        store
            .compare_and_put_reclamation_tombstone(&expected, record, references)
            .unwrap(),
        TombstoneWrite::Written
    );
}

#[test]
fn reclamation_snapshot_reads_tombstone_and_placements_together() {
    let (_dir, store) = store();
    let record = tombstone(1, 3);
    write_tombstone(&store, &record);
    let placement = placement(1);
    assert_eq!(
        store.compare_and_put_blob_placement(None, &placement).unwrap(),
        CompareWrite::Written
    );

    assert_eq!(
        store.reclamation_snapshot(&record.digest).unwrap(),
        ReclamationSnapshot {
            tombstone: Some(record),
            placements: vec![placement],
        }
    );
}

#[test]
fn reclamation_reads_empty_state_before_tables_exist() {
    let (_dir, store) = store();
    let record = tombstone(1, 1);

    assert_eq!(
        store.reclamation_snapshot(&record.digest).unwrap(),
        ReclamationSnapshot {
            tombstone: None,
            placements: Vec::new(),
        }
    );
    assert_eq!(store.reclamation_tombstone(&record.digest).unwrap(), None);
    assert!(store.reclamation_tombstones().unwrap().is_empty());
    assert!(!store.compare_and_remove_reclamation_tombstone(&record).unwrap());
}

#[test]
fn reclamation_write_rejects_changed_tombstone_evidence() {
    let (_dir, store) = store();
    let initial = store.reclamation_snapshot(&digest(1)).unwrap();
    write_tombstone(&store, &tombstone(1, 3));

    assert_eq!(
        store
            .compare_and_put_reclamation_tombstone(&initial, &tombstone(1, 4), 0)
            .unwrap(),
        TombstoneWrite::Conflict
    );
    assert_eq!(store.reclamation_tombstone(&digest(1)).unwrap(), Some(tombstone(1, 3)));
}

#[test]
fn reclamation_write_rejects_changed_placement_evidence() {
    let (_dir, store) = store();
    let initial = store.reclamation_snapshot(&digest(1)).unwrap();
    assert_eq!(
        store.compare_and_put_blob_placement(None, &placement(1)).unwrap(),
        CompareWrite::Written
    );

    assert_eq!(
        store
            .compare_and_put_reclamation_tombstone(&initial, &tombstone(1, 3), 0)
            .unwrap(),
        TombstoneWrite::Conflict
    );
    assert!(store.reclamation_tombstone(&digest(1)).unwrap().is_none());
}

#[test]
fn reclamation_remove_requires_the_complete_record() {
    let (_dir, store) = store();
    let record = tombstone(1, 3);
    write_tombstone(&store, &record);

    assert!(
        !store
            .compare_and_remove_reclamation_tombstone(&tombstone(1, 4))
            .unwrap()
    );
    assert!(store.compare_and_remove_reclamation_tombstone(&record).unwrap());
    assert!(!store.compare_and_remove_reclamation_tombstone(&record).unwrap());
}

#[test]
fn reclamation_tombstones_use_digest_order() {
    let (_dir, store) = store();
    write_tombstone(&store, &tombstone(3, 1));
    write_tombstone(&store, &tombstone(1, 1));

    assert_eq!(
        store
            .reclamation_tombstones()
            .unwrap()
            .into_iter()
            .map(|record| record.digest)
            .collect::<Vec<_>>(),
        [digest(1), digest(3)]
    );
}

#[test]
fn reclamation_tombstones_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let record = tombstone(1, 2);
    {
        let store = MetaStore::open(&path).unwrap();
        store.initialize_distributed_state().unwrap();
        write_tombstone(&store, &record);
    }

    let store = MetaStore::open_existing(path).unwrap();
    assert_eq!(store.reclamation_tombstone(&record.digest).unwrap(), Some(record));
}

#[test]
fn reclamation_write_rejects_a_verdict_proved_against_an_older_reference_revision() {
    let (_dir, store) = store();
    let expected = store.reclamation_snapshot(&digest(1)).unwrap();
    let references = store.reference_revision().unwrap();
    store.put_driver_value("published", b"reference").unwrap();

    assert_eq!(
        store
            .compare_and_put_reclamation_tombstone(&expected, &tombstone(1, 3), references)
            .unwrap(),
        TombstoneWrite::ReferencesMoved
    );
    assert_eq!(store.reclamation_tombstone(&digest(1)).unwrap(), None);
}
