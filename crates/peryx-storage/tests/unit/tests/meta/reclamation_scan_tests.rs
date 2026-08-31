use std::num::NonZeroUsize;

use peryx_ha::{
    ReclamationSnapshot, ReclamationState, ReclamationStore as _, ReclamationTombstone, ReclamationTombstonePage,
};
use peryx_identity::ArtifactDigest;

use crate::meta::MetaStore;

use super::store;

fn digest(suffix: u8) -> ArtifactDigest {
    ArtifactDigest::from_sha256(format!("{suffix:064x}")).unwrap()
}

fn limit(rows: usize) -> NonZeroUsize {
    NonZeroUsize::new(rows).unwrap()
}

fn write_tombstone(store: &MetaStore, suffix: u8) -> ReclamationTombstone {
    let record = ReclamationTombstone {
        digest: digest(suffix),
        state: ReclamationState::Pending,
        required_frontier: 7,
        fence: 1,
        attempts: 1,
        selected_at_unix: 10,
        updated_at_unix: 10,
    };
    let expected = ReclamationSnapshot {
        tombstone: None,
        placements: Vec::new(),
    };
    assert!(store.compare_and_put_reclamation_tombstone(&expected, &record).unwrap());
    record
}

#[test]
fn test_a_store_without_tombstones_pages_nothing() {
    let (_directory, store) = store();

    assert_eq!(
        store.scan_reclamation_tombstones(None, limit(2)).unwrap(),
        ReclamationTombstonePage::default()
    );
}

#[test]
fn test_a_page_stops_at_the_limit_and_names_its_last_digest() {
    let (_directory, store) = store();
    let first = write_tombstone(&store, 1);
    let second = write_tombstone(&store, 2);
    write_tombstone(&store, 3);

    assert_eq!(
        store.scan_reclamation_tombstones(None, limit(2)).unwrap(),
        ReclamationTombstonePage {
            records: vec![first, second],
            next_cursor: Some(digest(2).canonical()),
        }
    );
}

#[test]
fn test_the_cursor_resumes_at_the_following_tombstone() {
    let (_directory, store) = store();
    write_tombstone(&store, 1);
    write_tombstone(&store, 2);
    let third = write_tombstone(&store, 3);

    assert_eq!(
        store
            .scan_reclamation_tombstones(Some(&digest(2).canonical()), limit(2))
            .unwrap(),
        ReclamationTombstonePage {
            records: vec![third],
            next_cursor: None,
        }
    );
}

#[test]
fn test_a_page_that_exhausts_the_table_at_the_limit_omits_the_cursor() {
    let (_directory, store) = store();
    let first = write_tombstone(&store, 1);
    let second = write_tombstone(&store, 2);

    assert_eq!(
        store.scan_reclamation_tombstones(None, limit(2)).unwrap(),
        ReclamationTombstonePage {
            records: vec![first, second],
            next_cursor: None,
        }
    );
}

#[test]
fn test_removing_the_cursor_tombstone_does_not_block_progress() {
    let (_directory, store) = store();
    let second = write_tombstone(&store, 2);
    let third = write_tombstone(&store, 3);
    assert!(store.compare_and_remove_reclamation_tombstone(&second).unwrap());

    assert_eq!(
        store
            .scan_reclamation_tombstones(Some(&digest(2).canonical()), limit(2))
            .unwrap(),
        ReclamationTombstonePage {
            records: vec![third],
            next_cursor: None,
        }
    );
}

#[test]
fn test_a_tombstone_inserted_below_the_cursor_waits_for_the_next_cycle() {
    let (_directory, store) = store();
    write_tombstone(&store, 2);
    write_tombstone(&store, 1);

    assert_eq!(
        store
            .scan_reclamation_tombstones(Some(&digest(2).canonical()), limit(2))
            .unwrap(),
        ReclamationTombstonePage::default()
    );
}
