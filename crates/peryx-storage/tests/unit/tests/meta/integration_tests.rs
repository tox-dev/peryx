use peryx_ha::{
    ArtifactPlacement, ArtifactSource, BackendId, BackendLocation, BlobPlacementKey, BlobPlacementRecord,
    BlobPlacementState, ByteAvailability, CompareWrite, DataCenterId, ReclaimGuard, ReclaimGuardArm,
    ReclaimGuardStore as _, ReclamationSnapshot, ReclamationState, ReclamationStore as _, ReclamationTombstone,
};
use peryx_identity::ArtifactDigest;
use redb::{ReadableDatabase as _, TableHandle as _};
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Barrier};

use crate::blob::{CHUNK_BYTES, ChunkedDigest, Digest};
use crate::meta::{IntentAdmission, IntentLimits, IntentUsage, MetaError, MetaStore, NewReconcileEntry, TransferAudit};

const DISTRIBUTED_TABLES: [&str; 17] = [
    "artifact_placement",
    "blob_placement",
    "blob_chunk_digest",
    "blob_reclaim_guard",
    "derived_view_frontier",
    "ingress_intent",
    "ingress_intent_count",
    "ingress_intent_order",
    "ingress_intent_seq",
    "journal",
    "journal_blobs",
    "journal_mutations",
    "reclamation_tombstone",
    "reconcile_backlog",
    "transfer_audit",
    "visibility_snapshot",
    "writer",
];

#[test]
fn test_open_omits_distributed_domain_tables() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let store = MetaStore::open(&path).unwrap();
    store.initialize_distributed_state().unwrap();
    store.put_driver_value("local", b"value").unwrap();
    let digest = ArtifactDigest::from_sha256(Digest::of(b"blob").as_str()).unwrap();
    let placement = blob_placement(&digest);
    assert_eq!(store.get_artifact_placement(digest.canonical().as_str()).unwrap(), None);
    assert_eq!(store.blob_placement(&placement.key).unwrap(), None);
    assert_eq!(
        store.reclamation_snapshot(&digest).unwrap(),
        ReclamationSnapshot {
            tombstone: None,
            placements: Vec::new(),
        }
    );
    assert!(store.reclamation_tombstones().unwrap().is_empty());
    assert_eq!(store.reclaim_guard(digest.canonical().as_str()).unwrap(), None);
    assert_eq!(store.visibility_snapshot().unwrap(), None);
    assert_eq!(store.blob_chunk_digest(&digest).unwrap(), None);
    assert_eq!(store.staged_intent_usage("repo").unwrap(), IntentUsage::default());
    assert!(store.list_pending_intents(1).unwrap().is_empty());
    assert!(store.pending_reconcile(1).unwrap().is_empty());
    assert_eq!(store.reconcile_entry("west:1:1").unwrap(), None);
    assert_eq!(store.count_reconcile().unwrap(), 0);
    assert!(store.transfer_audits("repo").unwrap().is_empty());
    assert_eq!(store.view_frontier("search").unwrap(), None);
    assert!(store.view_frontiers().unwrap().is_empty());
    assert_eq!(store.writer_identity().unwrap(), None);
    assert!(store.journal_snapshot(0, 1).unwrap().records.is_empty());
    drop(store);

    assert_distributed_tables(&path, &[]);
}

#[test]
fn test_initialize_distributed_state_rejects_a_read_only_store() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    drop(MetaStore::open(&path).unwrap());
    let store = MetaStore::open_existing_read_only(path).unwrap();

    assert!(store.initialize_distributed_state().is_err());
}

#[test]
fn test_artifact_placement_first_write_creates_only_its_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let store = MetaStore::open(&path).unwrap();
    store
        .put_artifact_placement(
            "sha256:artifact",
            &ArtifactPlacement {
                source: ArtifactSource::Hosted,
                availability: ByteAvailability::Local,
            },
        )
        .unwrap();
    drop(store);

    assert_distributed_tables(&path, &["artifact_placement"]);
}

#[test]
fn test_blob_placement_first_write_creates_only_its_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let store = MetaStore::open(&path).unwrap();
    let digest = ArtifactDigest::from_sha256(Digest::of(b"blob").as_str()).unwrap();
    let placement = blob_placement(&digest);
    assert_eq!(
        store.compare_and_put_blob_placement(None, &placement).unwrap(),
        CompareWrite::Written
    );
    drop(store);

    assert_distributed_tables(&path, &["blob_placement"]);
}

#[test]
fn test_chunk_digest_first_write_creates_only_its_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let store = MetaStore::open(&path).unwrap();
    let digest = ArtifactDigest::from_sha256(Digest::of(b"blob").as_str()).unwrap();
    store
        .put_blob_chunk_digest(&digest, &ChunkedDigest::of(b"blob", CHUNK_BYTES))
        .unwrap();
    drop(store);

    assert_distributed_tables(&path, &["blob_chunk_digest"]);
}

#[test]
fn test_reclamation_first_write_creates_only_evidence_tables() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let store = MetaStore::open(&path).unwrap();
    let digest = ArtifactDigest::from_sha256(Digest::of(b"blob").as_str()).unwrap();
    let expected = store.reclamation_snapshot(&digest).unwrap();
    assert!(
        store
            .compare_and_put_reclamation_tombstone(
                &expected,
                &ReclamationTombstone {
                    digest,
                    state: ReclamationState::Pending,
                    required_frontier: 1,
                    fence: 1,
                    attempts: 1,
                    selected_at_unix: 0,
                    updated_at_unix: 0,
                },
            )
            .unwrap()
    );
    drop(store);

    assert_distributed_tables(&path, &["blob_placement", "reclamation_tombstone"]);
}

#[test]
fn test_reclaim_guard_first_write_creates_only_its_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let store = MetaStore::open(&path).unwrap();
    assert_eq!(
        store
            .compare_and_arm_reclaim_guards(&["sha256:blob"], 0, 0, ReclaimGuard { expires_at_unix: 1 })
            .unwrap(),
        ReclaimGuardArm::Armed(vec!["sha256:blob".to_owned()])
    );
    drop(store);

    assert_distributed_tables(&path, &["blob_reclaim_guard"]);
}

#[test]
fn test_ingress_first_write_creates_only_its_tables() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let store = MetaStore::open(&path).unwrap();
    store
        .stage_intent(
            IntentAdmission {
                authority: "repo",
                key: "upload",
                digest: "sha256:blob",
                size: 4,
                payload: b"blob",
            },
            IntentLimits {
                max_records: 1,
                max_bytes: 4,
                backpressure_percent: 80,
            },
            0,
        )
        .unwrap();
    drop(store);

    assert_distributed_tables(
        &path,
        &[
            "ingress_intent",
            "ingress_intent_count",
            "ingress_intent_order",
            "ingress_intent_seq",
        ],
    );
}

#[test]
fn test_reconcile_first_write_creates_only_its_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let store = MetaStore::open(&path).unwrap();
    store
        .enqueue_reconcile(
            &NewReconcileEntry {
                source: "west",
                epoch: 1,
                serial: 1,
                durably_committed: true,
                already_applied: false,
                superseded: false,
                traceparent: None,
            },
            0,
        )
        .unwrap();
    drop(store);

    assert_distributed_tables(&path, &["reconcile_backlog"]);
}

#[test]
fn test_transfer_first_write_creates_only_its_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let store = MetaStore::open(&path).unwrap();
    store.record_transfer_audit(&transfer_audit()).unwrap();
    drop(store);

    assert_distributed_tables(&path, &["transfer_audit"]);
}

#[test]
fn test_visibility_first_write_creates_only_its_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let store = MetaStore::open(&path).unwrap();
    store.save_visibility_snapshot(b"snapshot").unwrap();
    drop(store);

    assert_distributed_tables(&path, &["visibility_snapshot"]);
}

#[test]
fn test_replication_journal_first_write_creates_only_its_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let store = MetaStore::open(&path).unwrap();
    store
        .commit_driver_txn::<_, MetaError>(|_| Ok(((), vec![b"entry".to_vec()])))
        .unwrap();
    drop(store);

    assert_distributed_tables(&path, &["journal"]);
}

#[test]
fn test_writer_first_claim_creates_only_its_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let store = MetaStore::open(&path).unwrap();
    store.claim_writer_identity("node-a").unwrap();
    drop(store);

    assert_distributed_tables(&path, &["writer"]);
}

#[test]
fn test_frontier_first_write_creates_only_its_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let store = MetaStore::open(&path).unwrap();
    store.set_view_frontier("search", 1).unwrap();
    drop(store);

    assert_distributed_tables(&path, &["derived_view_frontier"]);
}

#[test]
fn test_concurrent_first_writes_initialize_an_optional_table_once() {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let barrier = Arc::new(Barrier::new(8));

    std::thread::scope(|scope| {
        let handles = (0..8)
            .map(|value| {
                let store = store.clone();
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait();
                    store.save_visibility_snapshot(&[value]).unwrap();
                })
            })
            .collect::<Vec<_>>();
        for handle in handles {
            handle.join().unwrap();
        }
    });

    let snapshot = store.visibility_snapshot().unwrap().unwrap();
    assert_eq!(snapshot.len(), 1);
    assert!((0..8).contains(&snapshot[0]));
}

#[test]
fn test_distributed_reads_reject_an_incompatible_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let db = redb::Database::create(&path).unwrap();
    let txn = db.begin_write().unwrap();
    txn.open_table(redb::TableDefinition::<&str, u64>::new("visibility_snapshot"))
        .unwrap();
    txn.commit().unwrap();
    drop(db);

    assert!(matches!(
        MetaStore::open_existing(path).unwrap().visibility_snapshot(),
        Err(MetaError::Table(redb::TableError::TableTypeMismatch { .. }))
    ));
}

#[test]
fn test_open_existing_requires_database_file() {
    let dir = tempfile::tempdir().unwrap();
    assert!(MetaStore::open_existing(dir.path().join("missing.redb")).is_err());
    assert!(MetaStore::open_existing_read_only(dir.path().join("missing.redb")).is_err());
}

#[test]
fn test_open_existing_read_only_reads_and_rejects_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let writable = MetaStore::open(&path).unwrap();
    assert!(format!("{writable:?}").contains("ReadWrite"));
    assert_eq!(writable.next_serial().unwrap(), 1);
    writable.analytics().save(b"snapshot").unwrap();
    drop(writable);

    let read_only = MetaStore::open_existing_read_only(path).unwrap();
    assert!(format!("{read_only:?}").contains("ReadOnly"));
    let analytics = read_only.analytics();

    assert_eq!(read_only.current_serial().unwrap(), 1);
    assert_eq!(analytics.load().unwrap(), Some(b"snapshot".to_vec()));
    assert_read_only(read_only.next_serial().unwrap_err());
    assert_read_only(analytics.save(b"changed").unwrap_err());
    drop(read_only);
    assert_eq!(analytics.load().unwrap(), None);
}

fn assert_read_only(err: MetaError) {
    assert!(matches!(
        err,
        MetaError::Transaction(redb::TransactionError::Storage(redb::StorageError::Io(err)))
            if err.kind() == std::io::ErrorKind::PermissionDenied && err.to_string() == "metadata store is read-only"
    ));
}

fn table_names(path: &Path) -> HashSet<String> {
    let db = redb::Database::open(path).unwrap();
    let read = db.begin_read().unwrap();
    read.list_tables()
        .unwrap()
        .map(|table| table.name().to_owned())
        .collect()
}

fn assert_distributed_tables(path: &Path, expected: &[&str]) {
    let tables = table_names(path);
    let actual = DISTRIBUTED_TABLES
        .into_iter()
        .filter(|name| tables.contains(*name))
        .collect::<HashSet<_>>();
    assert_eq!(actual, expected.iter().copied().collect());
}

fn blob_placement(digest: &ArtifactDigest) -> BlobPlacementRecord {
    BlobPlacementRecord {
        key: BlobPlacementKey {
            digest: digest.clone(),
            backend: BackendId::new("filesystem").unwrap(),
            data_center: DataCenterId::new("west").unwrap(),
            location: BackendLocation::new("blobs/01").unwrap(),
        },
        state: BlobPlacementState::Pending,
        fence: 1,
        generation: 1,
        updated_at_unix: 0,
    }
}

fn transfer_audit() -> TransferAudit {
    TransferAudit {
        authority: "repo".to_owned(),
        source: "east".to_owned(),
        target: "west".to_owned(),
        actor: "operator".to_owned(),
        reason: "maintenance".to_owned(),
        barrier: 1,
        epoch: 1,
        commit_index: 1,
    }
}
