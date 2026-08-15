use std::num::NonZeroUsize;
use std::str::FromStr as _;

use peryx_ha::{
    BackendId, BackendLocation, BlobPlacementGroupPage, BlobPlacementKey, BlobPlacementPage, BlobPlacementRecord,
    BlobPlacementState, BlobPlacementStore, CompareWrite, DataCenterId,
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

fn put(store: &MetaStore, key: BlobPlacementKey) -> BlobPlacementRecord {
    let record = BlobPlacementRecord {
        key,
        state: BlobPlacementState::Pending,
        fence: 1,
        generation: 1,
        updated_at_unix: 0,
    };
    assert_eq!(
        BlobPlacementStore::compare_and_put_blob_placement(store, None, &record).unwrap(),
        CompareWrite::Written
    );
    record
}

#[test]
fn test_row_scan_pages_in_key_order() {
    let (_directory, store) = store();
    let first_record = put(&store, key(1, "east/01"));
    let second_record = put(&store, key(2, "east/02"));

    let first = BlobPlacementStore::scan_blob_placements(&store, None, NonZeroUsize::new(1).unwrap()).unwrap();
    let second =
        BlobPlacementStore::scan_blob_placements(&store, first.next_cursor.as_deref(), NonZeroUsize::new(2).unwrap())
            .unwrap();

    assert_eq!(first.records, [first_record]);
    assert!(first.next_cursor.is_some());
    assert_eq!(second.records, [second_record]);
    assert_eq!(second.next_cursor, None);
}

#[test]
fn test_group_scan_keeps_digest_rows_together() {
    let (_directory, store) = store();
    let first = put(&store, key(1, "east/01"));
    let second = put(&store, key(1, "east/02"));
    let third = put(&store, key(2, "east/03"));
    let fourth = put(&store, key(3, "east/04"));

    let page = BlobPlacementStore::scan_blob_placement_groups(&store, None, NonZeroUsize::new(2).unwrap()).unwrap();
    let tail = BlobPlacementStore::scan_blob_placement_groups(
        &store,
        page.next_cursor.as_deref(),
        NonZeroUsize::new(1).unwrap(),
    )
    .unwrap();

    assert_eq!(page.groups, [vec![first, second], vec![third]]);
    assert_eq!(page.next_cursor, Some(digest(2).canonical()));
    assert_eq!(tail.groups, [vec![fourth]]);
    assert_eq!(tail.next_cursor, None);
}

#[test]
fn test_scans_return_empty_pages_before_the_first_write() {
    let (_directory, store) = store();
    let limit = NonZeroUsize::new(1).unwrap();

    assert_eq!(
        BlobPlacementStore::scan_blob_placements(&store, None, limit).unwrap(),
        BlobPlacementPage::default()
    );
    assert_eq!(
        BlobPlacementStore::scan_blob_placement_groups(&store, None, limit).unwrap(),
        BlobPlacementGroupPage::default()
    );
}

#[test]
fn test_blob_placement_reads_reject_an_incompatible_table() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("peryx.redb");
    let database = redb::Database::create(&path).unwrap();
    let transaction = database.begin_write().unwrap();
    transaction
        .open_table(redb::TableDefinition::<&str, u64>::new("blob_placement"))
        .unwrap();
    transaction.commit().unwrap();
    drop(database);
    let store = MetaStore::open_existing(path).unwrap();
    let key = key(1, "east/01");
    let limit = NonZeroUsize::new(1).unwrap();

    assert!(BlobPlacementStore::blob_placement(&store, &key).is_err());
    assert!(BlobPlacementStore::blob_placements(&store, &key.digest).is_err());
    assert!(BlobPlacementStore::scan_blob_placements(&store, None, limit).is_err());
    assert!(BlobPlacementStore::scan_blob_placement_groups(&store, None, limit).is_err());
}
