use std::collections::BTreeMap;
use std::str::FromStr as _;

use peryx_identity::{ArtifactDigest, RevocationReason, UserId};

use crate::meta::checkpoint::CheckpointState;
use crate::meta::fault::initialized;
use crate::meta::{
    Checkpoint, CheckpointIdentity, CheckpointVerifyError, DriverBlobReference, DriverMutation, MetaError, MetaStore,
};

const DIGEST_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000001";

fn identity() -> CheckpointIdentity {
    CheckpointIdentity {
        source: "primary-a".to_owned(),
        protocol_version: 1,
        schema_version: 7,
    }
}

fn store() -> MetaStore {
    let (store, _pages, _fault) = initialized();
    store
}

/// Drives the ordinary driver path, so the journal holds what a real write leaves rather than rows
/// a test planted.
fn commit(store: &MetaStore, body: impl FnOnce(&mut crate::meta::DriverTxn) -> Result<(), MetaError>) {
    store
        .commit_driver_txn(|txn| body(txn).map(|()| ((), vec![b"{}".to_vec()])))
        .unwrap();
}

fn put(store: &MetaStore, key: &str, value: &[u8]) {
    commit(store, |txn| txn.put(key, value));
}

fn delete(store: &MetaStore, key: &str) {
    commit(store, |txn| txn.remove(key).map(|_| ()));
}

fn put_local(store: &MetaStore, key: &str, value: &[u8]) {
    commit(store, |txn| txn.put_local(key, value));
}

fn folded(store: &MetaStore) -> CheckpointState {
    store.folded_state(store.current_serial().unwrap()).unwrap()
}

fn rows(pairs: &[(&str, &[u8])]) -> BTreeMap<String, Vec<u8>> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_vec()))
        .collect()
}

#[test]
fn test_a_fold_reproduces_what_ordinary_writes_left_behind() {
    let store = store();
    put(&store, "alpha", b"one");
    put(&store, "beta", b"two");
    put(&store, "alpha", b"three");
    delete(&store, "beta");

    assert_eq!(folded(&store).rows(), &rows(&[("alpha", b"three")]));
}

#[test]
fn test_a_fold_stops_at_the_serial_it_was_asked_for() {
    let store = store();
    put(&store, "alpha", b"one");
    let through = store.current_serial().unwrap();
    put(&store, "alpha", b"two");

    assert_eq!(store.folded_state(through).unwrap().rows(), &rows(&[("alpha", b"one")]),);
}

#[test]
fn test_a_local_row_stays_out_of_the_fold_a_replicated_row_stays_in() {
    let store = store();
    put(&store, "replicated", b"kept");
    put_local(&store, "derived", b"dropped");

    assert_eq!(folded(&store).rows(), &rows(&[("replicated", b"kept")]));
}

#[test]
fn test_a_local_write_over_a_replicated_key_leaves_the_replicated_value_in_the_fold() {
    let store = store();
    put(&store, "shared", b"replicated");
    put_local(&store, "shared", b"local");

    // The store holds what the local write left; the fold holds what replicated under the same key.
    assert_eq!(store.get_driver_value("shared").unwrap(), Some(b"local".to_vec()));
    assert_eq!(folded(&store).rows(), &rows(&[("shared", b"replicated")]));
}

#[test]
fn test_a_local_delete_of_a_replicated_key_leaves_the_replicated_row_in_the_fold() {
    let store = store();
    put(&store, "shared", b"replicated");
    commit(&store, |txn| txn.remove_local("shared").map(|_| ()));

    assert_eq!(store.get_driver_value("shared").unwrap(), None);
    assert_eq!(folded(&store).rows(), &rows(&[("shared", b"replicated")]));
}

#[test]
fn test_a_fold_carries_blob_references() {
    let store = store();
    commit(&store, |txn| {
        txn.put("alpha", b"payload")?;
        txn.reference_blob(DIGEST_HEX, 12);
        Ok(())
    });

    assert_eq!(
        folded(&store).blobs().iter().cloned().collect::<Vec<_>>(),
        vec![DriverBlobReference {
            sha256: DIGEST_HEX.to_owned(),
            size: 12,
        }],
    );
}

#[test]
fn test_a_fold_carries_the_revocation_row_the_writer_journalled() {
    let store = store();
    let digest = ArtifactDigest::from_str(&format!("sha256:{DIGEST_HEX}")).unwrap();
    let created = store
        .put_digest_revocation(
            &digest,
            &RevocationReason::new("incident").unwrap(),
            &UserId::random(),
            10,
        )
        .unwrap();

    assert_eq!(
        folded(&store).revocations(),
        &BTreeMap::from([(digest.canonical(), created.record().clone())]),
    );
}

#[test]
fn test_a_fold_keeps_the_last_revocation_row_for_a_digest() {
    let store = store();
    let digest = ArtifactDigest::from_str(&format!("sha256:{DIGEST_HEX}")).unwrap();
    let actor = UserId::random();
    store
        .put_digest_revocation(&digest, &RevocationReason::new("incident").unwrap(), &actor, 10)
        .unwrap();
    let lifted = store.lift_digest_revocation(&digest, &actor, 20).unwrap().unwrap();

    assert_eq!(
        folded(&store).revocations(),
        &BTreeMap::from([(digest.canonical(), lifted.record().clone())]),
    );
}

#[test]
fn test_an_incremental_fold_over_a_published_checkpoint_equals_a_fold_from_empty() {
    let store = store();
    put(&store, "alpha", b"one");
    put(&store, "beta", b"two");
    commit(&store, |txn| {
        txn.put("gamma", b"payload")?;
        txn.reference_blob(DIGEST_HEX, 3);
        Ok(())
    });
    store
        .put_digest_revocation(
            &ArtifactDigest::from_str(&format!("sha256:{DIGEST_HEX}")).unwrap(),
            &RevocationReason::new("incident").unwrap(),
            &UserId::random(),
            10,
        )
        .unwrap();
    let first = store.publish_checkpoint(identity()).unwrap();
    put(&store, "alpha", b"three");
    delete(&store, "beta");
    put(&store, "delta", b"four");

    let incremental = store.publish_checkpoint(identity()).unwrap();

    let from_empty = folded(&store);
    assert!(first.serial < incremental.serial);
    assert_eq!(store.checkpoint().unwrap().unwrap().state, from_empty);
    assert_eq!(incremental, from_empty.manifest(identity(), incremental.serial));
}

#[test]
fn test_publication_names_the_current_serial_and_verifies() {
    let store = store();
    put(&store, "alpha", b"one");

    let manifest = store.publish_checkpoint(identity()).unwrap();
    let checkpoint = store.checkpoint().unwrap().unwrap();

    assert_eq!(manifest.serial, store.current_serial().unwrap());
    assert_eq!((manifest.rows, manifest.identity.clone()), (1, identity()));
    assert_eq!(checkpoint.manifest, manifest);
    checkpoint.verify().unwrap();
}

#[test]
fn test_publication_on_an_empty_store_publishes_an_empty_checkpoint() {
    let store = store();

    let manifest = store.publish_checkpoint(identity()).unwrap();

    assert_eq!((manifest.serial, manifest.rows, manifest.bytes), (0, 0, 0));
    store.checkpoint().unwrap().unwrap().verify().unwrap();
}

#[test]
fn test_nothing_is_published_before_the_first_publication() {
    let store = store();

    assert_eq!(store.checkpoint_manifest().unwrap(), None);
    assert_eq!(store.checkpoint().unwrap(), None);
}

#[test]
fn test_a_republished_checkpoint_drops_the_rows_the_tail_deleted() {
    let store = store();
    put(&store, "alpha", b"one");
    store.publish_checkpoint(identity()).unwrap();
    delete(&store, "alpha");

    let manifest = store.publish_checkpoint(identity()).unwrap();

    assert_eq!(manifest.rows, 0);
    assert!(store.checkpoint().unwrap().unwrap().state.rows().is_empty());
}

#[test]
fn test_a_truncated_checkpoint_is_rejected_before_its_digest_is_recomputed() {
    let store = store();
    put(&store, "alpha", b"one");
    put(&store, "beta", b"two");
    store.publish_checkpoint(identity()).unwrap();
    let published = store.checkpoint().unwrap().unwrap();
    let mut short = published.state.rows().clone();
    short.remove("beta");

    let truncated = Checkpoint {
        manifest: published.manifest,
        state: CheckpointState::from_parts(
            short,
            published.state.revocations().clone(),
            published.state.blobs().clone(),
        ),
    };

    assert_eq!(
        truncated.verify(),
        Err(CheckpointVerifyError::Truncated {
            unit: "rows",
            declared: 2,
            actual: 1,
        }),
    );
}

#[test]
fn test_a_corrupted_row_fails_the_digest() {
    let store = store();
    put(&store, "alpha", b"one");
    store.publish_checkpoint(identity()).unwrap();
    let published = store.checkpoint().unwrap().unwrap();

    // Same length as the published value, so the count checks pass and only the digest can catch it.
    let corrupted = Checkpoint {
        manifest: published.manifest,
        state: CheckpointState::from_parts(
            rows(&[("alpha", b"two")]),
            published.state.revocations().clone(),
            published.state.blobs().clone(),
        ),
    };

    assert!(matches!(corrupted.verify(), Err(CheckpointVerifyError::Digest { .. })));
}

#[test]
fn test_publication_removes_no_journal_row_and_moves_no_serial() {
    let store = store();
    put(&store, "alpha", b"one");
    put(&store, "beta", b"two");
    let before = store.journal_after(0, 100).unwrap();

    store.publish_checkpoint(identity()).unwrap();

    assert_eq!(store.journal_after(0, 100).unwrap(), before);
    assert_eq!(store.current_serial().unwrap(), u64::try_from(before.len()).unwrap(),);
}

#[test]
fn test_the_canonical_encoding_separates_keys_a_concatenation_would_confuse() {
    let mut split = CheckpointState::default();
    split
        .apply(
            vec![
                DriverMutation::Put {
                    key: "ab".to_owned(),
                    value: b"c".to_vec(),
                },
                DriverMutation::Put {
                    key: "a".to_owned(),
                    value: b"bc".to_vec(),
                },
            ],
            Vec::new(),
            b"{}",
        )
        .unwrap();
    let mut single = CheckpointState::default();
    single
        .apply(
            vec![DriverMutation::Put {
                key: "abc".to_owned(),
                value: Vec::new(),
            }],
            Vec::new(),
            b"{}",
        )
        .unwrap();

    assert_ne!(split.canonical(), single.canonical());
}

/// Per row: the tag, the two length prefixes, and the key and value themselves.
const ROW_OVERHEAD_BYTES: u64 = 1 + 8 + 8;

#[test]
fn test_the_manifest_sizes_the_state_a_later_transfer_has_to_carry() {
    let store = store();
    let value = vec![b'v'; 4096];
    for index in 0..64_u64 {
        put(&store, &format!("project/{index:048}"), &value);
    }

    let manifest = store.publish_checkpoint(identity()).unwrap();

    // The encoding is a linear function of the rows, so #2127 can size chunks from the manifest
    // alone instead of measuring the payload it is about to stream.
    let key_bytes = 64 * (b"project/".len() as u64 + 48);
    assert_eq!(
        (manifest.rows, manifest.bytes),
        (64, 64 * (ROW_OVERHEAD_BYTES + 4096) + key_bytes),
    );
}
