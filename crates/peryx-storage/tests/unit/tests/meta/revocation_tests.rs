use std::str::FromStr as _;

use peryx_identity::{ArtifactDigest, RevocationReason, UserId};
use rstest::rstest;

use crate::meta::{
    DigestRevocation, DigestRevocationQuery, DigestRevocationQueryError, DigestRevocationState, DigestRevocationStatus,
    LiftRevocationOutcome, MetaStore, PutRevocationError, PutRevocationOutcome,
};

use super::store;

fn digest(suffix: u8) -> ArtifactDigest {
    ArtifactDigest::from_str(&format!("sha256:{suffix:064x}")).unwrap()
}

fn reason(value: &str) -> RevocationReason {
    RevocationReason::new(value).unwrap()
}

#[test]
fn test_digest_revocation_create_retry_conflict_lift_and_reopen() {
    let (_dir, store) = store();
    let digest = digest(1);
    let first_actor = UserId::random();
    let second_actor = UserId::random();
    assert!(!store.has_active_digest_revocation().unwrap());
    let created = store
        .put_digest_revocation(&digest, &reason("incident one"), &first_actor, 10)
        .unwrap();
    assert!(matches!(created, PutRevocationOutcome::Created(_)));
    assert_eq!(created.record().revision, 1);
    assert!(store.has_active_digest_revocation().unwrap());

    let retry = store
        .put_digest_revocation(&digest, &reason("incident one"), &second_actor, 11)
        .unwrap();
    assert!(matches!(retry, PutRevocationOutcome::Unchanged(_)));
    assert_eq!(retry.record(), created.record());
    assert!(matches!(
        store.put_digest_revocation(&digest, &reason("another incident"), &second_actor, 12),
        Err(PutRevocationError::ReasonConflict)
    ));

    let lifted = store
        .lift_digest_revocation(&digest, &second_actor, 13)
        .unwrap()
        .unwrap();
    assert!(matches!(lifted, LiftRevocationOutcome::Lifted(_)));
    assert_eq!(lifted.record().reason.as_str(), "incident one");
    assert_eq!(lifted.record().created_by, first_actor);
    assert_eq!(lifted.record().created_at_unix, 10);
    assert_eq!(lifted.record().revision, 2);
    assert_eq!(
        lifted.record().state,
        DigestRevocationState::Lifted {
            lifted_by: second_actor.clone(),
            lifted_at_unix: 13,
        }
    );
    assert!(!store.has_active_digest_revocation().unwrap());
    let lift_retry = store
        .lift_digest_revocation(&digest, &first_actor, 14)
        .unwrap()
        .unwrap();
    assert!(matches!(lift_retry, LiftRevocationOutcome::Unchanged(_)));
    assert_eq!(lift_retry.record(), lifted.record());

    let reopened = store
        .put_digest_revocation(&digest, &reason("new incident"), &second_actor, 15)
        .unwrap();
    assert!(matches!(reopened, PutRevocationOutcome::Reopened(_)));
    assert_eq!(reopened.record().revision, 3);
    assert_eq!(reopened.record().reason.as_str(), "new incident");
    assert_eq!(reopened.record().created_by, second_actor);
    assert_eq!(reopened.record().created_at_unix, 15);
    assert_eq!(reopened.record().state, DigestRevocationState::Active);
    assert!(store.has_active_digest_revocation().unwrap());
    assert_eq!(
        store.digest_revocation(&digest).unwrap(),
        Some(reopened.record().clone())
    );
}

#[test]
fn test_digest_revocation_lift_unknown_is_absent() {
    let (_dir, store) = store();
    assert_eq!(
        store.lift_digest_revocation(&digest(1), &UserId::random(), 10).unwrap(),
        None
    );
    assert_eq!(store.digest_revocation(&digest(1)).unwrap(), None);
    assert!(!store.has_active_digest_revocation().unwrap());
}

#[test]
fn test_digest_revocation_reads_reject_an_incompatible_record_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("incompatible.redb");
    let database = redb::Database::create(&path).unwrap();
    let txn = database.begin_write().unwrap();
    txn.open_table(redb::TableDefinition::<&str, u64>::new("digest_revocation"))
        .unwrap();
    txn.commit().unwrap();
    drop(database);
    let store = MetaStore::open_existing(path).unwrap();

    assert!(matches!(
        store.has_active_digest_revocation(),
        Err(crate::meta::MetaError::Table(_))
    ));
    assert!(matches!(
        store.digest_revocation(&digest(1)),
        Err(crate::meta::MetaError::Table(_))
    ));
}

#[test]
fn test_digest_revocation_query_filters_and_paginates_stably() {
    let (_dir, store) = store();
    let actor = UserId::random();
    for suffix in 1..=4 {
        store
            .put_digest_revocation(
                &digest(suffix),
                &reason(&format!("incident {suffix}")),
                &actor,
                suffix.into(),
            )
            .unwrap();
    }
    store.lift_digest_revocation(&digest(2), &actor, 10).unwrap();

    let exact = store
        .query_digest_revocations(&DigestRevocationQuery {
            status: Some(DigestRevocationStatus::Active),
            cursor: None,
            limit: 3,
        })
        .unwrap();
    assert_eq!((exact.revocations.len(), exact.next_cursor), (3, None));

    let first = store
        .query_digest_revocations(&DigestRevocationQuery {
            status: Some(DigestRevocationStatus::Active),
            cursor: None,
            limit: 2,
        })
        .unwrap();
    assert_eq!(
        first
            .revocations
            .iter()
            .map(|record| record.digest.clone())
            .collect::<Vec<_>>(),
        vec![digest(1), digest(3)]
    );
    assert_eq!(first.next_cursor, Some(digest(3).canonical()));
    let second = store
        .query_digest_revocations(&DigestRevocationQuery {
            status: Some(DigestRevocationStatus::Active),
            cursor: Some(digest(3)),
            limit: 2,
        })
        .unwrap();
    assert_eq!(second.revocations.len(), 1);
    assert_eq!(second.revocations[0].digest, digest(4));
    assert_eq!(second.next_cursor, None);
    assert_eq!(
        store
            .query_digest_revocations(&DigestRevocationQuery {
                status: Some(DigestRevocationStatus::Lifted),
                ..DigestRevocationQuery::default()
            })
            .unwrap()
            .revocations[0]
            .digest,
        digest(2)
    );
}

#[rstest]
#[case(0)]
#[case(101)]
fn test_digest_revocation_query_rejects_invalid_limits(#[case] limit: usize) {
    let (_dir, store) = store();
    assert!(matches!(
        store.query_digest_revocations(&DigestRevocationQuery {
            limit,
            ..DigestRevocationQuery::default()
        }),
        Err(DigestRevocationQueryError::InvalidLimit)
    ));
}

#[test]
fn test_digest_revocation_rows_survive_restart() {
    let (dir, store) = store();
    let digest = digest(1);
    store
        .put_digest_revocation(&digest, &reason("incident"), &UserId::random(), 10)
        .unwrap();
    drop(store);

    let reopened = MetaStore::open_existing(dir.path().join("peryx.redb")).unwrap();
    assert_eq!(
        reopened.digest_revocation(&digest).unwrap().unwrap().reason.as_str(),
        "incident"
    );
}

#[test]
fn test_digest_revocation_old_store_reads_as_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    redb::Database::create(&path).unwrap();
    let store = MetaStore::open_existing(path).unwrap();

    assert_eq!(store.digest_revocation(&digest(1)).unwrap(), None);
    assert!(!store.has_active_digest_revocation().unwrap());
    assert_eq!(
        store
            .query_digest_revocations(&DigestRevocationQuery::default())
            .unwrap()
            .revocations,
        Vec::new()
    );
    assert_eq!(
        store.lift_digest_revocation(&digest(1), &UserId::random(), 10).unwrap(),
        None
    );
}

#[test]
fn test_digest_revocation_query_fails_closed_on_a_store_type_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let database = redb::Database::create(&path).unwrap();
    let txn = database.begin_write().unwrap();
    txn.open_table(redb::TableDefinition::<&str, u64>::new("digest_revocation"))
        .unwrap();
    txn.commit().unwrap();
    drop(database);

    assert!(matches!(
        MetaStore::open_existing(path)
            .unwrap()
            .query_digest_revocations(&DigestRevocationQuery::default()),
        Err(DigestRevocationQueryError::Store(_))
    ));
}

#[test]
fn test_digest_revocation_rejects_active_count_overflow() {
    let (dir, store) = store();
    drop(store);
    let database = redb::Database::open(dir.path().join("peryx.redb")).unwrap();
    let txn = database.begin_write().unwrap();
    txn.open_table(redb::TableDefinition::<&str, u64>::new("digest_revocation_state"))
        .unwrap()
        .insert("active_count", u64::MAX)
        .unwrap();
    txn.commit().unwrap();
    drop(database);
    let store = MetaStore::open_existing(dir.path().join("peryx.redb")).unwrap();

    assert!(matches!(
        store.put_digest_revocation(&digest(1), &reason("incident"), &UserId::random(), 10),
        Err(PutRevocationError::Store(crate::meta::MetaError::DriverPrecondition(message)))
            if message == "digest revocation active count overflow"
    ));
}

#[test]
fn test_digest_revocation_rejects_active_count_underflow() {
    let (dir, store) = store();
    let digest = digest(1);
    store
        .put_digest_revocation(&digest, &reason("incident"), &UserId::random(), 10)
        .unwrap();
    drop(store);
    let database = redb::Database::open(dir.path().join("peryx.redb")).unwrap();
    let txn = database.begin_write().unwrap();
    txn.open_table(redb::TableDefinition::<&str, u64>::new("digest_revocation_state"))
        .unwrap()
        .insert("active_count", 0)
        .unwrap();
    txn.commit().unwrap();
    drop(database);
    let store = MetaStore::open_existing(dir.path().join("peryx.redb")).unwrap();

    assert!(matches!(
        store.lift_digest_revocation(&digest, &UserId::random(), 11),
        Err(crate::meta::MetaError::DriverPrecondition(message))
            if message == "digest revocation active count underflow"
    ));
}

#[test]
fn test_digest_revocation_active_count_fails_closed_on_a_store_type_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let database = redb::Database::create(&path).unwrap();
    let txn = database.begin_write().unwrap();
    txn.open_table(redb::TableDefinition::<&str, &str>::new("digest_revocation_state"))
        .unwrap();
    txn.commit().unwrap();
    drop(database);

    assert!(
        MetaStore::open_existing(path)
            .unwrap()
            .has_active_digest_revocation()
            .is_err()
    );
}

#[rstest]
#[case::records_only(true, false)]
#[case::state_only(false, true)]
fn test_digest_revocation_active_count_fails_closed_on_an_incomplete_index(
    #[case] records_table: bool,
    #[case] state_table: bool,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let database = redb::Database::create(&path).unwrap();
    let txn = database.begin_write().unwrap();
    if records_table {
        txn.open_table(redb::TableDefinition::<&str, &[u8]>::new("digest_revocation"))
            .unwrap();
    }
    if state_table {
        txn.open_table(redb::TableDefinition::<&str, u64>::new("digest_revocation_state"))
            .unwrap();
    }
    txn.commit().unwrap();
    drop(database);

    assert!(matches!(
        MetaStore::open_existing(path)
            .unwrap()
            .has_active_digest_revocation(),
        Err(crate::meta::MetaError::DriverPrecondition(message))
            if message == "digest revocation index is incomplete"
    ));
}

fn write_raw_revocation_rows(path: &std::path::Path, records: &[DigestRevocation]) {
    let database = redb::Database::create(path).unwrap();
    let txn = database.begin_write().unwrap();
    {
        let mut table = txn
            .open_table(redb::TableDefinition::<&str, &[u8]>::new("digest_revocation"))
            .unwrap();
        for record in records {
            table
                .insert(
                    record.digest.canonical().as_str(),
                    serde_json::to_vec(record).unwrap().as_slice(),
                )
                .unwrap();
        }
    }
    txn.commit().unwrap();
    drop(database);
}

fn revocation(suffix: u8, state: DigestRevocationState) -> DigestRevocation {
    DigestRevocation {
        digest: digest(suffix),
        reason: reason("incident"),
        created_by: UserId::random(),
        created_at_unix: 10,
        state,
        revision: 1,
    }
}

#[test]
fn test_digest_revocation_open_backfills_active_count_for_pre_index_store() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let actor = UserId::random();
    write_raw_revocation_rows(
        &path,
        &[
            revocation(1, DigestRevocationState::Active),
            revocation(2, DigestRevocationState::Active),
            revocation(
                3,
                DigestRevocationState::Lifted {
                    lifted_by: actor,
                    lifted_at_unix: 20,
                },
            ),
        ],
    );

    let store = MetaStore::open(&path).unwrap();

    assert!(store.has_active_digest_revocation().unwrap());
    store.lift_digest_revocation(&digest(1), &UserId::random(), 30).unwrap();
    assert!(store.has_active_digest_revocation().unwrap());
    store.lift_digest_revocation(&digest(2), &UserId::random(), 31).unwrap();
    assert!(!store.has_active_digest_revocation().unwrap());
}

#[test]
fn test_digest_revocation_open_skips_a_corrupt_row_without_failing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    write_raw_revocation_rows(&path, &[revocation(1, DigestRevocationState::Active)]);
    // Malformed rows must not prevent store startup or inflate the backfill count.
    {
        let database = redb::Database::open(&path).unwrap();
        let txn = database.begin_write().unwrap();
        {
            let mut table = txn
                .open_table(redb::TableDefinition::<&str, &[u8]>::new("digest_revocation"))
                .unwrap();
            table
                .insert(digest(2).canonical().as_str(), b"not json".as_slice())
                .unwrap();
        }
        txn.commit().unwrap();
    }

    let store = MetaStore::open(&path).unwrap();

    assert!(store.has_active_digest_revocation().unwrap());
    store.lift_digest_revocation(&digest(1), &UserId::random(), 30).unwrap();
    assert!(!store.has_active_digest_revocation().unwrap());
}

#[test]
fn test_digest_revocation_open_leaves_a_consistent_count_untouched() {
    let (dir, store) = store();
    store
        .put_digest_revocation(&digest(1), &reason("incident"), &UserId::random(), 10)
        .unwrap();
    drop(store);

    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    assert!(store.has_active_digest_revocation().unwrap());
    store.lift_digest_revocation(&digest(1), &UserId::random(), 11).unwrap();
    assert!(!store.has_active_digest_revocation().unwrap());
}

#[test]
fn test_digest_revocation_open_reconciles_a_drifted_active_count() {
    let (dir, store) = store();
    let path = dir.path().join("peryx.redb");
    store
        .put_digest_revocation(&digest(1), &reason("incident"), &UserId::random(), 10)
        .unwrap();
    drop(store);
    let database = redb::Database::open(&path).unwrap();
    let txn = database.begin_write().unwrap();
    txn.open_table(redb::TableDefinition::<&str, u64>::new("digest_revocation_state"))
        .unwrap()
        .insert("active_count", 5)
        .unwrap();
    txn.commit().unwrap();
    drop(database);

    let store = MetaStore::open(&path).unwrap();

    assert!(store.has_active_digest_revocation().unwrap());
    store.lift_digest_revocation(&digest(1), &UserId::random(), 11).unwrap();
    assert!(!store.has_active_digest_revocation().unwrap());
}
