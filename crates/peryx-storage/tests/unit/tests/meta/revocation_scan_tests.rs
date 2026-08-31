use std::path::Path;
use std::str::FromStr as _;

use peryx_identity::{ArtifactDigest, RevocationReason, UserId};

use crate::meta::{
    DigestRevocation, DigestRevocationPage, DigestRevocationQuery, DigestRevocationQueryError, DigestRevocationState,
    DigestRevocationStatus, MetaError, MetaStore,
};

use super::store;

const RECORDS: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("digest_revocation");
const INDEX: redb::TableDefinition<&str, ()> = redb::TableDefinition::new("digest_revocation_by_status");

fn digest(suffix: u8) -> ArtifactDigest {
    ArtifactDigest::from_str(&format!("sha256:{suffix:064x}")).unwrap()
}

fn revoke(store: &MetaStore, suffix: u8, actor: &UserId) -> DigestRevocation {
    store
        .put_digest_revocation(
            &digest(suffix),
            &RevocationReason::new(format!("incident {suffix}")).unwrap(),
            actor,
            i64::from(suffix),
        )
        .unwrap()
        .record()
        .clone()
}

fn lift(store: &MetaStore, suffix: u8, actor: &UserId) -> DigestRevocation {
    store
        .lift_digest_revocation(&digest(suffix), actor, 100)
        .unwrap()
        .unwrap()
        .record()
        .clone()
}

fn page(
    store: &MetaStore,
    status: Option<DigestRevocationStatus>,
    cursor: Option<ArtifactDigest>,
    limit: usize,
) -> DigestRevocationPage {
    store
        .query_digest_revocations(&DigestRevocationQuery { status, cursor, limit })
        .unwrap()
}

fn filtered(
    store: &MetaStore,
    status: DigestRevocationStatus,
) -> Result<DigestRevocationPage, DigestRevocationQueryError> {
    store.query_digest_revocations(&DigestRevocationQuery {
        status: Some(status),
        ..DigestRevocationQuery::default()
    })
}

fn write_rows(path: &Path, rows: &[(String, Vec<u8>)]) {
    let database = redb::Database::create(path).unwrap();
    let txn = database.begin_write().unwrap();
    {
        let mut table = txn.open_table(RECORDS).unwrap();
        for (key, value) in rows {
            table.insert(key.as_str(), value.as_slice()).unwrap();
        }
    }
    txn.commit().unwrap();
}

fn write_index_keys(path: &Path, keys: &[String]) {
    let database = redb::Database::create(path).unwrap();
    let txn = database.begin_write().unwrap();
    {
        let mut table = txn.open_table(INDEX).unwrap();
        for key in keys {
            table.insert(key.as_str(), ()).unwrap();
        }
    }
    txn.commit().unwrap();
}

fn index_key(status: &str, suffix: u8) -> String {
    format!("{status}\0{}", digest(suffix).canonical())
}

#[test]
fn test_status_filtered_pages_never_decode_rows_in_the_other_status() {
    let (dir, store) = store();
    let path = dir.path().join("peryx.redb");
    let actor = UserId::random();
    for suffix in 1..=3 {
        revoke(&store, suffix, &actor);
        lift(&store, suffix, &actor);
    }
    let active = (4..=6).map(|suffix| revoke(&store, suffix, &actor)).collect::<Vec<_>>();
    drop(store);
    write_rows(
        &path,
        &(1..=3)
            .map(|suffix| (digest(suffix).canonical(), b"not json".to_vec()))
            .collect::<Vec<_>>(),
    );
    let store = MetaStore::open_existing(&path).unwrap();

    let first = page(&store, Some(DigestRevocationStatus::Active), None, 2);
    let second = page(&store, Some(DigestRevocationStatus::Active), Some(digest(5)), 2);

    assert_eq!(
        (first, second),
        (
            DigestRevocationPage {
                revocations: vec![active[0].clone(), active[1].clone()],
                next_cursor: Some(digest(5).canonical()),
            },
            DigestRevocationPage {
                revocations: vec![active[2].clone()],
                next_cursor: None,
            }
        )
    );
}

#[test]
fn test_status_filtered_pages_fail_closed_on_a_corrupt_row_in_their_own_status() {
    let (dir, store) = store();
    let path = dir.path().join("peryx.redb");
    let actor = UserId::random();
    revoke(&store, 1, &actor);
    lift(&store, 1, &actor);
    revoke(&store, 2, &actor);
    drop(store);
    write_rows(&path, &[(digest(1).canonical(), b"not json".to_vec())]);
    let store = MetaStore::open_existing(&path).unwrap();

    assert!(matches!(
        filtered(&store, DigestRevocationStatus::Lifted),
        Err(DigestRevocationQueryError::Store(MetaError::Decode(_)))
    ));
}

#[test]
fn test_status_filtered_pages_fail_closed_without_the_status_index() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    write_rows(&path, &[]);
    let store = MetaStore::open_existing(&path).unwrap();

    assert!(matches!(
        filtered(&store, DigestRevocationStatus::Active),
        Err(DigestRevocationQueryError::Store(MetaError::DriverPrecondition(message)))
            if message == "digest revocation index is incomplete"
    ));
}

#[test]
fn test_status_filtered_pages_fail_closed_on_an_index_entry_without_a_row() {
    let (dir, store) = store();
    let path = dir.path().join("peryx.redb");
    revoke(&store, 1, &UserId::random());
    drop(store);
    write_index_keys(&path, &[index_key("active", 2)]);
    let store = MetaStore::open_existing(&path).unwrap();

    assert!(matches!(
        filtered(&store, DigestRevocationStatus::Active),
        Err(DigestRevocationQueryError::Store(MetaError::DriverPrecondition(message)))
            if message == "digest revocation index references a missing row"
    ));
}

#[test]
fn test_open_builds_the_status_index_for_a_pre_index_store() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let actor = UserId::random();
    let lifted = DigestRevocation {
        digest: digest(1),
        reason: RevocationReason::new("incident").unwrap(),
        created_by: actor.clone(),
        created_at_unix: 10,
        state: DigestRevocationState::Lifted {
            lifted_by: actor.clone(),
            lifted_at_unix: 20,
        },
        revision: 2,
    };
    let active = DigestRevocation {
        digest: digest(2),
        reason: RevocationReason::new("incident").unwrap(),
        created_by: actor,
        created_at_unix: 11,
        state: DigestRevocationState::Active,
        revision: 1,
    };
    write_rows(
        &path,
        &[&lifted, &active].map(|record| (record.digest.canonical(), serde_json::to_vec(record).unwrap())),
    );

    let store = MetaStore::open(&path).unwrap();

    assert_eq!(
        (
            filtered(&store, DigestRevocationStatus::Active).unwrap(),
            filtered(&store, DigestRevocationStatus::Lifted).unwrap(),
        ),
        (
            DigestRevocationPage {
                revocations: vec![active],
                next_cursor: None,
            },
            DigestRevocationPage {
                revocations: vec![lifted],
                next_cursor: None,
            }
        )
    );
}

#[test]
fn test_open_drops_a_stale_status_index_entry() {
    let (dir, store) = store();
    let path = dir.path().join("peryx.redb");
    let active = revoke(&store, 1, &UserId::random());
    drop(store);
    write_index_keys(&path, &[index_key("lifted", 2)]);

    let store = MetaStore::open(&path).unwrap();

    assert_eq!(
        (
            filtered(&store, DigestRevocationStatus::Active).unwrap(),
            filtered(&store, DigestRevocationStatus::Lifted).unwrap(),
        ),
        (
            DigestRevocationPage {
                revocations: vec![active],
                next_cursor: None,
            },
            DigestRevocationPage {
                revocations: Vec::new(),
                next_cursor: None,
            }
        )
    );
}

#[test]
fn test_unfiltered_pages_walk_both_statuses_in_digest_order() {
    let (_dir, store) = store();
    let actor = UserId::random();
    let first_record = revoke(&store, 1, &actor);
    revoke(&store, 2, &actor);
    let second_record = lift(&store, 2, &actor);
    let third_record = revoke(&store, 3, &actor);

    let first = page(&store, None, None, 2);
    let second = page(&store, None, Some(digest(2)), 2);

    assert_eq!(
        (first, second),
        (
            DigestRevocationPage {
                revocations: vec![first_record, second_record],
                next_cursor: Some(digest(2).canonical()),
            },
            DigestRevocationPage {
                revocations: vec![third_record],
                next_cursor: None,
            }
        )
    );
}
