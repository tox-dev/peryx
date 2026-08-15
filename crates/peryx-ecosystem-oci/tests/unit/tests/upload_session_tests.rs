use uuid::Uuid;

use peryx_storage::meta::{
    AccountingClass, MetaError, MetaStore, NewQuotaReservation, QuotaError, QuotaLimits, QuotaValue,
};

use crate::upload_session::{UploadRecord, UploadStore as _};

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, store)
}

const MEMBERSHIP: &str = "oci:store:app:sha256:layer";

fn reserve(meta: &MetaStore, bytes: u64) -> Uuid {
    meta.reserve_quota(
        NewQuotaReservation {
            repository: "store",
            resource: Some("app"),
            group: None,
            digest: "sha256:layer",
            bytes,
            class: AccountingClass::Hosted,
            created_at_unix: 10,
        },
        QuotaLimits::default(),
    )
    .unwrap()
    .id
}

fn membership(txn: &mut peryx_storage::meta::DriverTxn) -> Result<((), Vec<Vec<u8>>), MetaError> {
    txn.put(MEMBERSHIP, &[])?;
    Ok(((), Vec::new()))
}

fn membership_fault(_txn: &mut peryx_storage::meta::DriverTxn) -> Result<((), Vec<Vec<u8>>), MetaError> {
    Err(MetaError::DriverPrecondition("membership commit failed".to_owned()))
}

#[test]
fn test_begin_opens_a_session_at_offset_zero() {
    let (_dir, store) = store();

    store.begin_upload("session-1", "hosted", "app/image", 5).unwrap();

    assert_eq!(
        store.upload_record("session-1").unwrap(),
        Some(UploadRecord {
            offset: 0,
            index: "hosted".to_owned(),
            name: "app/image".to_owned(),
            updated_at_unix: 5,
        })
    );
}

#[test]
fn test_advance_records_the_new_offset() {
    let (_dir, store) = store();
    store.begin_upload("session-1", "hosted", "app/image", 5).unwrap();

    assert!(store.advance_upload("session-1", 4096, 9).unwrap());

    let record = store.upload_record("session-1").unwrap().unwrap();
    assert_eq!((record.offset, record.updated_at_unix), (4096, 9));
}

#[test]
fn test_advancing_an_unknown_session_reports_no_change() {
    let (_dir, store) = store();

    assert!(!store.advance_upload("ghost", 10, 1).unwrap());
    assert_eq!(store.upload_record("ghost").unwrap(), None);
}

#[test]
fn test_remove_closes_the_session_once() {
    let (_dir, store) = store();
    store.begin_upload("session-1", "hosted", "app/image", 5).unwrap();

    assert!(store.remove_upload("session-1").unwrap());
    assert!(!store.remove_upload("session-1").unwrap());
    assert_eq!(store.upload_record("session-1").unwrap(), None);
}

#[test]
fn test_reclaim_removes_idle_sessions_and_keeps_fresh_ones() {
    let (_dir, store) = store();
    store.begin_upload("idle", "hosted", "app/image", 50).unwrap();
    store.begin_upload("fresh", "hosted", "app/other", 100).unwrap();

    assert_eq!(store.reclaim_uploads(50, 10).unwrap(), vec!["idle".to_owned()]);
    assert_eq!(store.upload_record("idle").unwrap(), None);
    assert!(store.upload_record("fresh").unwrap().is_some());
}

#[test]
fn test_reclaim_honors_the_limit() {
    let (_dir, store) = store();
    store.begin_upload("a", "hosted", "app/a", 1).unwrap();
    store.begin_upload("b", "hosted", "app/b", 1).unwrap();

    assert_eq!(store.reclaim_uploads(50, 1).unwrap().len(), 1);
    assert_eq!(
        ["a", "b"]
            .iter()
            .filter(|session| store.upload_record(session).unwrap().is_some())
            .count(),
        1
    );
}

#[test]
fn test_closing_upload_commits_the_rows_and_shuts_the_session() {
    let (_dir, store) = store();
    store.begin_upload("session-1", "hosted", "app/image", 5).unwrap();

    store
        .commit_driver_txn_closing_upload(Some("session-1"), membership)
        .unwrap();

    assert_eq!(store.get_driver_value(MEMBERSHIP).unwrap(), Some(Vec::new()));
    assert_eq!(store.upload_record("session-1").unwrap(), None);
}

#[test]
fn test_closing_upload_keeps_the_session_when_membership_fails() {
    let (_dir, store) = store();
    store.begin_upload("session-1", "hosted", "app/image", 5).unwrap();

    let result = store.commit_driver_txn_closing_upload(Some("session-1"), membership_fault);

    assert!(matches!(result, Err(MetaError::DriverPrecondition(_))));
    assert!(store.upload_record("session-1").unwrap().is_some());
    assert_eq!(store.get_driver_value(MEMBERSHIP).unwrap(), None);
}

#[test]
fn test_closing_upload_retry_after_a_failure_converges_on_one_membership() {
    let (_dir, store) = store();
    store.begin_upload("session-1", "hosted", "app/image", 5).unwrap();
    store
        .commit_driver_txn_closing_upload(Some("session-1"), membership_fault)
        .unwrap_err();

    store
        .commit_driver_txn_closing_upload(Some("session-1"), membership)
        .unwrap();

    assert_eq!(store.get_driver_value(MEMBERSHIP).unwrap(), Some(Vec::new()));
    assert_eq!(store.upload_record("session-1").unwrap(), None);
}

#[test]
fn test_closing_upload_without_a_session_commits_the_rows() {
    let (_dir, store) = store();

    store.commit_driver_txn_closing_upload(None, membership).unwrap();

    assert_eq!(store.get_driver_value(MEMBERSHIP).unwrap(), Some(Vec::new()));
}

#[test]
fn test_metered_closing_upload_commits_quota_and_shuts_the_session() {
    let (_dir, store) = store();
    let id = reserve(&store, 7);
    store.begin_upload("session-1", "store", "app", 5).unwrap();

    store
        .commit_driver_txn_with_quota_closing_upload(id, Some("session-1"), |txn| {
            txn.put(MEMBERSHIP, &[])?;
            Ok::<_, QuotaError>(((), Vec::new()))
        })
        .unwrap();

    assert_eq!(store.get_driver_value(MEMBERSHIP).unwrap(), Some(Vec::new()));
    assert_eq!(store.upload_record("session-1").unwrap(), None);
    assert_eq!(
        store.quota_usage("store").unwrap().accounted_bytes,
        QuotaValue {
            committed: 7,
            reserved: 0,
        }
    );
}

#[test]
fn test_metered_closing_upload_keeps_the_session_when_membership_fails() {
    let (_dir, store) = store();
    let id = reserve(&store, 7);
    store.begin_upload("session-1", "store", "app", 5).unwrap();

    let result = store.commit_driver_txn_with_quota_closing_upload(id, Some("session-1"), |txn| {
        txn.put(MEMBERSHIP, &[])?;
        Err::<((), Vec<Vec<u8>>), _>(QuotaError::Store(MetaError::DriverPrecondition(
            "membership failed".to_owned(),
        )))
    });

    assert!(result.is_err());
    assert!(store.upload_record("session-1").unwrap().is_some());
    assert_eq!(store.get_driver_value(MEMBERSHIP).unwrap(), None);
    assert_eq!(
        store.quota_usage("store").unwrap().accounted_bytes,
        QuotaValue {
            committed: 0,
            reserved: 7,
        }
    );
}

#[test]
fn test_metered_closing_upload_rejects_an_already_committed_reservation() {
    let (_dir, store) = store();
    let id = reserve(&store, 7);
    store.commit_quota_reservation(id).unwrap();
    store.begin_upload("session-1", "store", "app", 5).unwrap();

    let result = store.commit_driver_txn_with_quota_closing_upload(id, Some("session-1"), |txn| {
        txn.put(MEMBERSHIP, &[])?;
        Ok::<_, QuotaError>(((), Vec::new()))
    });

    assert!(matches!(result, Err(QuotaError::ReservationUnavailable { .. })));
    assert!(store.upload_record("session-1").unwrap().is_some());
    assert_eq!(store.get_driver_value(MEMBERSHIP).unwrap(), None);
}

#[test]
fn test_metered_closing_upload_retry_converges_on_one_membership_and_charge() {
    let (_dir, store) = store();
    let id = reserve(&store, 7);
    store.begin_upload("session-1", "store", "app", 5).unwrap();
    store
        .commit_driver_txn_with_quota_closing_upload(id, Some("session-1"), |txn| {
            txn.put(MEMBERSHIP, &[])?;
            Err::<((), Vec<Vec<u8>>), _>(QuotaError::Store(MetaError::DriverPrecondition(
                "first attempt".to_owned(),
            )))
        })
        .unwrap_err();

    store
        .commit_driver_txn_with_quota_closing_upload(id, Some("session-1"), |txn| {
            txn.put(MEMBERSHIP, &[])?;
            Ok::<_, QuotaError>(((), Vec::new()))
        })
        .unwrap();

    assert_eq!(store.get_driver_value(MEMBERSHIP).unwrap(), Some(Vec::new()));
    assert_eq!(store.upload_record("session-1").unwrap(), None);
    assert_eq!(
        store.quota_usage("store").unwrap().accounted_bytes,
        QuotaValue {
            committed: 7,
            reserved: 0,
        }
    );
}

#[test]
fn test_metered_closing_upload_without_a_session_commits_the_rows() {
    let (_dir, store) = store();
    let id = reserve(&store, 7);

    store
        .commit_driver_txn_with_quota_closing_upload(id, None, |txn| {
            txn.put(MEMBERSHIP, &[])?;
            Ok::<_, QuotaError>(((), Vec::new()))
        })
        .unwrap();

    assert_eq!(store.get_driver_value(MEMBERSHIP).unwrap(), Some(Vec::new()));
    assert_eq!(
        store.quota_usage("store").unwrap().accounted_bytes,
        QuotaValue {
            committed: 7,
            reserved: 0,
        }
    );
}
