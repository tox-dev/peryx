use std::num::NonZeroUsize;
use std::str::FromStr as _;

use peryx_identity::{ArtifactDigest, RevocationReason, UserId};
use peryx_storage::meta::{
    DigestRevocation, DigestRevocationPage, DigestRevocationQuery, DigestRevocationStatus, MetaError, MetaStore,
};

use crate::{Change, ChangePage, MetadataMutation, PROTOCOL_VERSION, Replica, SyncError};

const SOURCE: &str = "primary-a";
const INDEX: redb::TableDefinition<&str, ()> = redb::TableDefinition::new("digest_revocation_by_status");

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, meta)
}

fn digest(suffix: u8) -> ArtifactDigest {
    ArtifactDigest::from_str(&format!("sha256:{suffix:064x}")).unwrap()
}

fn revoke(meta: &MetaStore, suffix: u8, actor: &UserId) -> DigestRevocation {
    meta.put_digest_revocation(
        &digest(suffix),
        &RevocationReason::new(format!("incident {suffix}")).unwrap(),
        actor,
        i64::from(suffix),
    )
    .unwrap()
    .record()
    .clone()
}

fn lift(meta: &MetaStore, suffix: u8, actor: &UserId) -> DigestRevocation {
    meta.lift_digest_revocation(&digest(suffix), actor, 100)
        .unwrap()
        .unwrap()
        .record()
        .clone()
}

/// Projects the primary's journal exactly as the change feed does, then round-trips it through the
/// wire encoding so the replica reads what a peer would have sent it.
fn published(meta: &MetaStore, after: u64) -> ChangePage {
    let snapshot = meta.journal_snapshot(after, 16).unwrap();
    let page = ChangePage {
        version: PROTOCOL_VERSION,
        source: SOURCE.to_owned(),
        after,
        current_serial: snapshot.current_serial,
        changes: snapshot
            .records
            .into_iter()
            .map(|record| Change {
                serial: record.serial,
                event: record.payload,
                metadata: record.mutations.into_iter().map(Into::into).collect(),
                blobs: record.blobs.into_iter().map(Into::into).collect(),
            })
            .collect(),
    };
    serde_json::from_slice(&serde_json::to_vec(&page).unwrap()).unwrap()
}

fn forged(changes: Vec<Change>) -> ChangePage {
    ChangePage {
        version: PROTOCOL_VERSION,
        source: SOURCE.to_owned(),
        after: 0,
        current_serial: changes.len() as u64,
        changes,
    }
}

fn apply(meta: &MetaStore, page: ChangePage) -> Result<Vec<ArtifactDigest>, SyncError> {
    Replica::new(meta, NonZeroUsize::new(16).unwrap())
        .apply_page(page)
        .map(|applied| applied.revocations)
}

fn filtered(meta: &MetaStore, status: DigestRevocationStatus) -> DigestRevocationPage {
    meta.query_digest_revocations(&DigestRevocationQuery {
        status: Some(status),
        ..DigestRevocationQuery::default()
    })
    .unwrap()
}

fn revoked_and_lifted(meta: &MetaStore) -> (DigestRevocation, DigestRevocation, DigestRevocation) {
    let actor = UserId::random();
    let first = revoke(meta, 1, &actor);
    revoke(meta, 2, &actor);
    let second = lift(meta, 2, &actor);
    let third = revoke(meta, 3, &actor);
    (first, second, third)
}

#[test]
fn test_a_replica_serves_the_status_filtered_pages_the_primary_serves() {
    let (_primary_dir, primary) = store();
    let (_replica_dir, replica) = store();
    let (first, second, third) = revoked_and_lifted(&primary);

    apply(&replica, published(&primary, 0)).unwrap();

    assert_eq!(
        (
            filtered(&replica, DigestRevocationStatus::Active),
            filtered(&replica, DigestRevocationStatus::Lifted),
        ),
        (
            DigestRevocationPage {
                revocations: vec![first, third],
                next_cursor: None,
            },
            DigestRevocationPage {
                revocations: vec![second],
                next_cursor: None,
            }
        )
    );
}

#[test]
fn test_a_replica_reports_the_digests_a_page_revoked() {
    let (_primary_dir, primary) = store();
    let (_replica_dir, replica) = store();
    revoked_and_lifted(&primary);

    let revocations = apply(&replica, published(&primary, 0)).unwrap();

    assert_eq!(revocations, vec![digest(1), digest(2), digest(2), digest(3)]);
}

#[test]
fn test_a_replica_answers_a_revoked_digest_the_way_the_primary_does() {
    let (_primary_dir, primary) = store();
    let (_replica_dir, replica) = store();
    revoked_and_lifted(&primary);

    apply(&replica, published(&primary, 0)).unwrap();

    assert_eq!(
        (
            replica.digest_revocation(&digest(1)).unwrap(),
            replica.has_active_digest_revocation().unwrap(),
        ),
        (
            primary.digest_revocation(&digest(1)).unwrap(),
            primary.has_active_digest_revocation().unwrap(),
        )
    );
}

#[test]
fn test_a_replica_rebuilds_a_status_index_written_before_the_index_existed() {
    let (_primary_dir, primary) = store();
    let replica_dir = tempfile::tempdir().unwrap();
    let path = replica_dir.path().join("peryx.redb");
    let replica = MetaStore::open(&path).unwrap();
    let (first, second, third) = revoked_and_lifted(&primary);
    apply(&replica, published(&primary, 0)).unwrap();
    drop(replica);
    let database = redb::Database::create(&path).unwrap();
    let txn = database.begin_write().unwrap();
    txn.delete_table(INDEX).unwrap();
    txn.commit().unwrap();
    drop(database);

    let replica = MetaStore::open(&path).unwrap();

    assert_eq!(
        (
            filtered(&replica, DigestRevocationStatus::Active),
            filtered(&replica, DigestRevocationStatus::Lifted),
        ),
        (
            DigestRevocationPage {
                revocations: vec![first, third],
                next_cursor: None,
            },
            DigestRevocationPage {
                revocations: vec![second],
                next_cursor: None,
            }
        )
    );
}

#[test]
fn test_a_replica_applying_a_revocation_twice_leaves_the_active_count_alone() {
    let (_primary_dir, primary) = store();
    let (_replica_dir, replica) = store();
    let actor = UserId::random();
    let record = revoke(&primary, 1, &actor);
    let mut published = published(&primary, 0);
    let entry = published.changes.remove(0).event;
    let replay = |serial: u64| Change {
        serial,
        event: entry.clone(),
        metadata: Vec::new(),
        blobs: Vec::new(),
    };

    apply(&replica, forged(vec![replay(1)])).unwrap();
    let mut again = forged(vec![replay(2)]);
    again.after = 1;
    again.current_serial = 2;
    apply(&replica, again).unwrap();

    assert_eq!(
        (
            filtered(&replica, DigestRevocationStatus::Active),
            replica.has_active_digest_revocation().unwrap(),
        ),
        (
            DigestRevocationPage {
                revocations: vec![record],
                next_cursor: None,
            },
            true
        )
    );
}

#[test]
fn test_a_replica_rejects_a_core_entry_it_cannot_decode() {
    let (_dir, replica) = store();
    let page = forged(vec![Change {
        serial: 1,
        event: br#"{"server-op":"digest-revocation"}"#.to_vec(),
        metadata: Vec::new(),
        blobs: Vec::new(),
    }]);

    let result = apply(&replica, page);

    assert!(matches!(result, Err(SyncError::Store(MetaError::Decode(_)))));
    assert_eq!(replica.current_serial().unwrap(), 0);
}

#[test]
fn test_a_replica_leaves_an_ecosystem_payload_to_its_driver() {
    let (_dir, replica) = store();
    let page = forged(vec![Change {
        serial: 1,
        event: br#"{"action":"add-file","project":"alpha"}"#.to_vec(),
        metadata: vec![MetadataMutation::Put {
            key: "pypi\u{0}p\u{0}alpha".to_owned(),
            value: b"{}".to_vec(),
        }],
        blobs: Vec::new(),
    }]);

    let revocations = apply(&replica, page).unwrap();

    assert_eq!(
        (revocations, filtered(&replica, DigestRevocationStatus::Active)),
        (
            Vec::new(),
            DigestRevocationPage {
                revocations: Vec::new(),
                next_cursor: None,
            }
        )
    );
}
