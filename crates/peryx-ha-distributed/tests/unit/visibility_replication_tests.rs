use std::num::NonZeroUsize;
use std::path::Path;
use std::str::FromStr as _;

use peryx_identity::{ArtifactDigest, RevocationReason, UserId};
use peryx_storage::meta::{DigestRevocation, MetaError, MetaStore};

use crate::{Change, ChangePage, PROTOCOL_VERSION, Replica, ReplicaState, SyncError};

const SOURCE: &str = "primary-a";
/// The record an ecosystem rewrites to hide one of its own artifacts, opaque to every crate that moves it.
const ARTIFACT_ROW: &str = "pypi\u{0}u\u{0}hosted/flask/flask-1.0-py3-none-any.whl";
const TRASHED: &[u8] = br#"{"file":"flask-1.0-py3-none-any.whl","trashed":{"deleted_at_unix":7}}"#;

fn digest() -> ArtifactDigest {
    ArtifactDigest::from_str(&format!("sha256:{:064x}", 0x5eu8)).unwrap()
}

fn open(path: &Path) -> MetaStore {
    MetaStore::open(path.join("peryx.redb")).unwrap()
}

/// The hidden set as a node answers it: the ecosystem's own record, and the server's revocation row.
fn hidden(meta: &MetaStore) -> (Option<Vec<u8>>, Option<DigestRevocation>, bool) {
    (
        meta.get_driver_value(ARTIFACT_ROW).unwrap(),
        meta.digest_revocation(&digest()).unwrap(),
        meta.has_active_digest_revocation().unwrap(),
    )
}

/// Hides the artifact along both dimensions: the ecosystem trashes its record, the server revokes the
/// digest. Each takes a serial, so one page carries both to a follower.
fn hide(meta: &MetaStore) {
    meta.commit_driver_txn(|txn| {
        txn.put(ARTIFACT_ROW, TRASHED)?;
        Ok::<_, MetaError>(((), vec![br#"{"action":"remove-file","project":"flask"}"#.to_vec()]))
    })
    .unwrap();
    meta.put_digest_revocation(
        &digest(),
        &RevocationReason::new("compromised build host").unwrap(),
        &UserId::random(),
        7,
    )
    .unwrap();
}

/// Projects the primary's journal exactly as the change feed does, then round-trips it through the wire
/// encoding so the replica reads what a peer would have sent it.
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

fn apply(meta: &MetaStore, page: ChangePage) -> Result<u64, SyncError> {
    Replica::new(meta, NonZeroUsize::new(16).unwrap())
        .apply_page(page)
        .map(|applied| applied.outcome.serial)
}

fn cursor(meta: &MetaStore) -> Option<ReplicaState> {
    Replica::new(meta, NonZeroUsize::new(16).unwrap()).state().unwrap()
}

#[test]
fn test_a_replica_hides_what_the_primary_hides_once_the_page_applies() {
    let primary_dir = tempfile::tempdir().unwrap();
    let replica_dir = tempfile::tempdir().unwrap();
    let primary = open(primary_dir.path());
    let replica = open(replica_dir.path());
    hide(&primary);

    apply(&replica, published(&primary, 0)).unwrap();

    assert_eq!(
        hidden(&replica),
        (
            Some(TRASHED.to_vec()),
            primary.digest_revocation(&digest()).unwrap(),
            true
        )
    );
}

#[test]
fn test_a_replica_advertises_the_serial_it_committed_the_hidden_set_at() {
    let primary_dir = tempfile::tempdir().unwrap();
    let replica_dir = tempfile::tempdir().unwrap();
    let primary = open(primary_dir.path());
    let replica = open(replica_dir.path());
    hide(&primary);

    let serial = apply(&replica, published(&primary, 0)).unwrap();

    assert_eq!(
        (serial, cursor(&replica)),
        (
            primary.current_serial().unwrap(),
            Some(ReplicaState {
                source: SOURCE.to_owned(),
                serial: primary.current_serial().unwrap(),
            })
        )
    );
}

#[test]
fn test_a_restarted_replica_still_hides_what_it_committed() {
    let primary_dir = tempfile::tempdir().unwrap();
    let replica_dir = tempfile::tempdir().unwrap();
    let primary = open(primary_dir.path());
    let replica = open(replica_dir.path());
    hide(&primary);
    apply(&replica, published(&primary, 0)).unwrap();
    drop(replica);

    let restarted = open(replica_dir.path());

    assert_eq!(
        (hidden(&restarted), cursor(&restarted)),
        (
            hidden(&primary),
            Some(ReplicaState {
                source: SOURCE.to_owned(),
                serial: primary.current_serial().unwrap(),
            })
        )
    );
}

#[test]
fn test_a_page_a_replica_cannot_fully_apply_leaves_the_hidden_set_and_the_cursor_alone() {
    let primary_dir = tempfile::tempdir().unwrap();
    let replica_dir = tempfile::tempdir().unwrap();
    let primary = open(primary_dir.path());
    let replica = open(replica_dir.path());
    hide(&primary);
    let mut page = published(&primary, 0);
    page.changes.push(Change {
        serial: page.current_serial + 1,
        event: br#"{"server-op":"digest-revocation"}"#.to_vec(),
        metadata: Vec::new(),
        blobs: Vec::new(),
    });
    page.current_serial += 1;

    let result = apply(&replica, page);

    assert!(matches!(result, Err(SyncError::Store(MetaError::Decode(_)))));
    assert_eq!((hidden(&replica), cursor(&replica)), ((None, None, false), None));
}
