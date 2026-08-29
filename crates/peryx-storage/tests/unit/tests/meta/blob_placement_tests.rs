use std::str::FromStr as _;

use peryx_ha::{
    BackendId, BackendLocation, BlobPlacementKey, BlobPlacementRecord, BlobPlacementState, BlobPlacementStore,
    CompareWrite, DataCenterId, MAX_PLACEMENTS_PER_DIGEST,
};
use peryx_identity::ArtifactDigest;

use crate::meta::MetaStore;

use super::store;

fn digest(suffix: u8) -> ArtifactDigest {
    ArtifactDigest::from_str(&format!("sha256:{suffix:064x}")).unwrap()
}

fn key(suffix: u8, location: &str) -> BlobPlacementKey {
    BlobPlacementKey {
        digest: digest(suffix),
        backend: BackendId::new("filesystem").unwrap(),
        data_center: DataCenterId::new("east").unwrap(),
        location: BackendLocation::new(location).unwrap(),
    }
}

fn record(key: BlobPlacementKey, generation: u64) -> BlobPlacementRecord {
    BlobPlacementRecord {
        key,
        state: BlobPlacementState::Pending,
        fence: 3,
        transfer_attempt: 1,
        generation,
        updated_at_unix: generation.cast_signed(),
    }
}

fn put(store: &MetaStore, record: &BlobPlacementRecord) -> CompareWrite {
    BlobPlacementStore::compare_and_put_blob_placement(store, None, record).unwrap()
}

#[test]
fn test_blob_placement_reads_are_empty_before_the_first_write() {
    let (_directory, store) = store();
    let key = key(1, "east/01");

    assert_eq!(BlobPlacementStore::blob_placement(&store, &key).unwrap(), None);
    assert!(
        BlobPlacementStore::blob_placements(&store, &key.digest)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn test_compare_and_put_inserts_and_reads_a_record() {
    let (_directory, store) = store();
    let record = record(key(1, "east/01"), 1);

    assert_eq!(put(&store, &record), CompareWrite::Written);
    assert_eq!(
        BlobPlacementStore::blob_placement(&store, &record.key).unwrap(),
        Some(record)
    );
}

#[test]
fn test_compare_and_put_rejects_a_stale_expected_record() {
    let (_directory, store) = store();
    let stored = record(key(1, "east/01"), 1);
    assert_eq!(put(&store, &stored), CompareWrite::Written);
    let stale = record(stored.key.clone(), 0);
    let replacement = record(stored.key.clone(), 2);

    assert_eq!(
        BlobPlacementStore::compare_and_put_blob_placement(&store, Some(&stale), &replacement).unwrap(),
        CompareWrite::Conflict
    );
    assert_eq!(
        BlobPlacementStore::blob_placement(&store, &stored.key).unwrap(),
        Some(stored)
    );
}

#[test]
fn test_compare_and_put_replaces_the_matching_record() {
    let (_directory, store) = store();
    let stored = record(key(1, "east/01"), 1);
    assert_eq!(put(&store, &stored), CompareWrite::Written);
    let replacement = BlobPlacementRecord {
        state: BlobPlacementState::Verified { size: 4_096 },
        generation: 2,
        updated_at_unix: 20,
        ..stored.clone()
    };

    assert_eq!(
        BlobPlacementStore::compare_and_put_blob_placement(&store, Some(&stored), &replacement).unwrap(),
        CompareWrite::Written
    );
    assert_eq!(
        BlobPlacementStore::blob_placement(&store, &replacement.key).unwrap(),
        Some(replacement)
    );
}

#[test]
fn test_digest_reads_filter_and_sort_by_encoded_key() {
    let (_directory, store) = store();
    for record in [
        record(key(1, "east/03"), 3),
        record(key(2, "east/00"), 4),
        record(key(1, "east/01"), 1),
        record(key(1, "east/02"), 2),
    ] {
        assert_eq!(put(&store, &record), CompareWrite::Written);
    }

    assert_eq!(
        BlobPlacementStore::blob_placements(&store, &digest(1))
            .unwrap()
            .into_iter()
            .map(|record| record.key.location)
            .collect::<Vec<_>>(),
        [
            BackendLocation::new("east/01").unwrap(),
            BackendLocation::new("east/02").unwrap(),
            BackendLocation::new("east/03").unwrap(),
        ]
    );
}

#[test]
fn test_digest_capacity_is_enforced_atomically() {
    let (_directory, store) = store();
    for index in 0..MAX_PLACEMENTS_PER_DIGEST {
        assert_eq!(
            put(&store, &record(key(1, &format!("east/{index:02}")), index as u64)),
            CompareWrite::Written
        );
    }

    assert_eq!(
        put(&store, &record(key(1, "east/overflow"), 65)),
        CompareWrite::CapacityExceeded
    );
    assert_eq!(
        BlobPlacementStore::blob_placements(&store, &digest(1)).unwrap().len(),
        MAX_PLACEMENTS_PER_DIGEST
    );
}

#[test]
fn test_committed_replacement_survives_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("peryx.redb");
    let stored = record(key(1, "east/01"), 1);
    let replacement = BlobPlacementRecord {
        state: BlobPlacementState::Verified { size: 512 },
        generation: 2,
        updated_at_unix: 30,
        ..stored.clone()
    };
    {
        let store = MetaStore::open(&path).unwrap();
        assert_eq!(put(&store, &stored), CompareWrite::Written);
        assert_eq!(
            BlobPlacementStore::compare_and_put_blob_placement(&store, Some(&stored), &replacement).unwrap(),
            CompareWrite::Written
        );
    }

    let reopened = MetaStore::open_existing(path).unwrap();
    assert_eq!(
        BlobPlacementStore::blob_placement(&reopened, &replacement.key).unwrap(),
        Some(replacement)
    );
}
