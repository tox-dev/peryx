use tempfile::TempDir;

use peryx_ha::TransferAuditStore;

use crate::meta::{MetaStore, TransferAudit};

fn store() -> (TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, store)
}

fn audit(authority: &str, commit_index: u64) -> TransferAudit {
    TransferAudit {
        authority: authority.to_owned(),
        source: "east".to_owned(),
        target: "west".to_owned(),
        actor: "alice".to_owned(),
        reason: "drain east".to_owned(),
        barrier: 42,
        epoch: 3,
        commit_index,
    }
}

#[test]
fn test_a_recorded_audit_reads_back_whole() {
    let (_dir, store) = store();
    let record = audit("proj", 9);
    store.record_transfer_audit(&record).unwrap();

    assert_eq!(store.transfer_audits("proj").unwrap(), vec![record]);
}

#[test]
fn test_an_authority_with_no_transfer_reads_empty() {
    let (_dir, store) = store();
    assert_eq!(store.transfer_audits("proj").unwrap(), Vec::new());
}

#[test]
fn test_repeated_transfers_read_in_commit_order() {
    let (_dir, store) = store();
    store.record_transfer_audit(&audit("proj", 30)).unwrap();
    store.record_transfer_audit(&audit("proj", 4)).unwrap();
    store.record_transfer_audit(&audit("proj", 12)).unwrap();

    let indices: Vec<u64> = store
        .transfer_audits("proj")
        .unwrap()
        .iter()
        .map(|record| record.commit_index)
        .collect();
    assert_eq!(indices, vec![4, 12, 30]);
}

#[test]
fn test_re_recording_the_same_commit_books_one_line() {
    let (_dir, store) = store();
    store.record_transfer_audit(&audit("proj", 9)).unwrap();
    let mut retried = audit("proj", 9);
    retried.reason = "drain east (retry)".to_owned();
    store.record_transfer_audit(&retried).unwrap();

    assert_eq!(store.transfer_audits("proj").unwrap(), vec![retried]);
}

#[test]
fn test_audits_are_scoped_to_their_authority() {
    let (_dir, store) = store();
    store.record_transfer_audit(&audit("proj-a", 1)).unwrap();
    store.record_transfer_audit(&audit("proj-b", 2)).unwrap();

    let a = store.transfer_audits("proj-a").unwrap();
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].authority, "proj-a");
    assert_eq!(a[0].commit_index, 1);
}

#[test]
fn test_transfer_audit_trait_round_trips_a_record() {
    let (_dir, store) = store();
    let record = audit("proj", 1);

    <MetaStore as TransferAuditStore>::record_transfer_audit(&store, &record).unwrap();

    assert_eq!(
        <MetaStore as TransferAuditStore>::transfer_audits(&store, "proj").unwrap(),
        vec![record]
    );
}
