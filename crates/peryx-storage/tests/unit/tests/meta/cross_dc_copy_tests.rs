use std::str::FromStr as _;

use peryx_identity::ArtifactDigest;

use crate::meta::{
    BackendId, BackendLocation, BlobPlacementFailure, BlobPlacementKey, BlobPlacementTransition, CopyBacklogEntry,
    CopyBacklogError, CopyPlan, CrossDcCopy, DataCenterId, MAX_COPY_BACKLOG_BATCH, MetaStore, VerifiedSource,
    plan_cross_dc_copy,
};

const FENCE: u64 = 5;

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, store)
}

fn digest(suffix: u8) -> ArtifactDigest {
    ArtifactDigest::from_str(&format!("sha256:{suffix:064x}")).unwrap()
}

fn key(suffix: u8, data_center: &str, location: &str) -> BlobPlacementKey {
    BlobPlacementKey {
        digest: digest(suffix),
        backend: BackendId::new("filesystem").unwrap(),
        data_center: DataCenterId::new(data_center).unwrap(),
        location: BackendLocation::new(location).unwrap(),
    }
}

fn dc(name: &str) -> DataCenterId {
    DataCenterId::new(name).unwrap()
}

fn stage(store: &MetaStore, key: &BlobPlacementKey) {
    store
        .apply_blob_placement(key, &BlobPlacementTransition::Stage, FENCE, 0)
        .unwrap();
}

fn verify(store: &MetaStore, key: &BlobPlacementKey, size: u64) {
    stage(store, key);
    store
        .apply_blob_placement(
            key,
            &BlobPlacementTransition::Verify {
                observed: key.digest.clone(),
                size,
            },
            FENCE,
            0,
        )
        .unwrap();
}

fn digests(page: &[CopyBacklogEntry]) -> Vec<ArtifactDigest> {
    page.iter().map(|entry| entry.digest.clone()).collect()
}

#[test]
fn test_scan_backlog_selects_a_digest_a_peer_verifies_but_the_local_dc_lacks() {
    let (_dir, store) = store();
    verify(&store, &key(1, "east", "east/01"), 128);

    let page = store.scan_cross_dc_copy_backlog(&dc("west"), None, 10).unwrap();

    assert_eq!(digests(&page.entries), vec![digest(1)]);
    assert_eq!(page.entries[0].sources.len(), 1);
    assert_eq!(page.entries[0].sources[0].key, key(1, "east", "east/01"));
    assert_eq!(page.entries[0].sources[0].size, 128);
    assert_eq!(page.scanned, 1);
    assert_eq!(page.next_cursor, None);
}

#[test]
fn test_scan_backlog_skips_a_digest_the_local_dc_already_verifies() {
    let (_dir, store) = store();
    verify(&store, &key(1, "east", "east/01"), 128);
    verify(&store, &key(1, "west", "west/01"), 128);

    let page = store.scan_cross_dc_copy_backlog(&dc("west"), None, 10).unwrap();

    assert!(page.entries.is_empty());
    assert_eq!(page.scanned, 1);
}

#[test]
fn test_scan_backlog_skips_a_digest_with_a_local_transfer_in_flight() {
    let (_dir, store) = store();
    verify(&store, &key(1, "east", "east/01"), 128);
    stage(&store, &key(1, "west", "west/01"));

    let page = store.scan_cross_dc_copy_backlog(&dc("west"), None, 10).unwrap();

    assert!(page.entries.is_empty());
}

#[test]
fn test_scan_backlog_skips_a_digest_the_local_dc_revoked() {
    let (_dir, store) = store();
    verify(&store, &key(1, "east", "east/01"), 128);
    let revoked = key(1, "west", "west/01");
    stage(&store, &revoked);
    store
        .apply_blob_placement(&revoked, &BlobPlacementTransition::Revoke, FENCE, 0)
        .unwrap();

    let page = store.scan_cross_dc_copy_backlog(&dc("west"), None, 10).unwrap();

    assert!(page.entries.is_empty());
}

#[test]
fn test_scan_backlog_retries_a_failed_local_placement() {
    let (_dir, store) = store();
    verify(&store, &key(1, "east", "east/01"), 128);
    let failed = key(1, "west", "west/01");
    stage(&store, &failed);
    store
        .apply_blob_placement(
            &failed,
            &BlobPlacementTransition::Fail {
                class: BlobPlacementFailure::SourceUnavailable,
            },
            FENCE,
            0,
        )
        .unwrap();

    let page = store.scan_cross_dc_copy_backlog(&dc("west"), None, 10).unwrap();

    // A failed local attempt is not a settled copy, so the digest still owes a retry.
    assert_eq!(digests(&page.entries), vec![digest(1)]);
}

#[test]
fn test_scan_backlog_skips_a_digest_with_no_verified_peer() {
    let (_dir, store) = store();
    stage(&store, &key(1, "east", "east/01"));

    let page = store.scan_cross_dc_copy_backlog(&dc("west"), None, 10).unwrap();

    assert!(page.entries.is_empty());
    assert_eq!(page.scanned, 1);
}

#[test]
fn test_scan_backlog_reports_every_verified_peer_as_a_source() {
    let (_dir, store) = store();
    verify(&store, &key(1, "east", "east/01"), 128);
    verify(&store, &key(1, "south", "south/01"), 128);

    let page = store.scan_cross_dc_copy_backlog(&dc("west"), None, 10).unwrap();

    let sources: Vec<BlobPlacementKey> = page.entries[0]
        .sources
        .iter()
        .map(|source| source.key.clone())
        .collect();
    assert_eq!(sources, vec![key(1, "east", "east/01"), key(1, "south", "south/01")]);
}

#[test]
fn test_scan_backlog_is_empty_on_an_empty_ledger() {
    let (_dir, store) = store();

    let page = store.scan_cross_dc_copy_backlog(&dc("west"), None, 10).unwrap();

    assert_eq!(
        page,
        crate::meta::CopyBacklogPage {
            entries: Vec::new(),
            scanned: 0,
            next_cursor: None,
        }
    );
}

#[test]
fn test_scan_backlog_paginates_and_resumes_after_the_cursor() {
    let (_dir, store) = store();
    verify(&store, &key(1, "east", "east/01"), 1);
    verify(&store, &key(2, "east", "east/02"), 2);
    verify(&store, &key(3, "east", "east/03"), 3);

    let first = store.scan_cross_dc_copy_backlog(&dc("west"), None, 2).unwrap();
    assert_eq!(digests(&first.entries), vec![digest(1), digest(2)]);
    assert_eq!(first.scanned, 2);
    assert_eq!(first.next_cursor, Some(digest(2).canonical()));

    let second = store
        .scan_cross_dc_copy_backlog(&dc("west"), first.next_cursor.as_deref(), 2)
        .unwrap();
    assert_eq!(digests(&second.entries), vec![digest(3)]);
    assert_eq!(second.scanned, 1);
    assert_eq!(second.next_cursor, None);
}

#[test]
fn test_scan_backlog_counts_scanned_digests_including_settled_ones() {
    let (_dir, store) = store();
    verify(&store, &key(1, "west", "west/01"), 1);
    verify(&store, &key(2, "east", "east/02"), 2);

    let page = store.scan_cross_dc_copy_backlog(&dc("west"), None, 10).unwrap();

    assert_eq!(digests(&page.entries), vec![digest(2)]);
    assert_eq!(page.scanned, 2);
}

#[test]
fn test_scan_backlog_rejects_an_out_of_range_limit() {
    let (_dir, store) = store();

    let low = store.scan_cross_dc_copy_backlog(&dc("west"), None, 0);
    assert!(matches!(low, Err(CopyBacklogError::InvalidLimit)));

    let high = store.scan_cross_dc_copy_backlog(&dc("west"), None, MAX_COPY_BACKLOG_BATCH + 1);
    assert!(matches!(high, Err(CopyBacklogError::InvalidLimit)));

    assert_eq!(
        CopyBacklogError::InvalidLimit.to_string(),
        format!("limit must be between 1 and {MAX_COPY_BACKLOG_BATCH}")
    );
}

fn source(suffix: u8, data_center: &str, location: &str, generation: u64, size: u64) -> VerifiedSource {
    VerifiedSource {
        key: key(suffix, data_center, location),
        generation,
        size,
    }
}

fn entry(sources: Vec<VerifiedSource>) -> CopyBacklogEntry {
    CopyBacklogEntry {
        digest: digest(1),
        sources,
    }
}

fn plan(entry: &CopyBacklogEntry, fence: u64) -> CopyPlan {
    plan_cross_dc_copy(
        entry,
        &dc("west"),
        &BackendId::new("filesystem").unwrap(),
        &BackendLocation::new("west/01").unwrap(),
        fence,
    )
}

#[test]
fn test_plan_copies_from_the_verified_peer_into_the_local_placement() {
    let plan = plan(&entry(vec![source(1, "east", "east/01", 2, 128)]), FENCE);

    assert_eq!(
        plan,
        CopyPlan::Copy(Box::new(CrossDcCopy {
            target: key(1, "west", "west/01"),
            source: key(1, "east", "east/01"),
            size: 128,
            fence: FENCE,
        }))
    );
}

#[test]
fn test_plan_prefers_the_highest_generation_source() {
    let plan = plan(
        &entry(vec![
            source(1, "east", "east/01", 2, 10),
            source(1, "south", "south/01", 7, 20),
        ]),
        FENCE,
    );

    let CopyPlan::Copy(copy) = plan else { panic!("{plan:?}") };
    assert_eq!(copy.source, key(1, "south", "south/01"));
    assert_eq!(copy.size, 20);
}

#[test]
fn test_plan_breaks_generation_ties_toward_the_earliest_source() {
    let plan = plan(
        &entry(vec![
            source(1, "east", "east/01", 4, 10),
            source(1, "south", "south/01", 4, 20),
        ]),
        FENCE,
    );

    let CopyPlan::Copy(copy) = plan else { panic!("{plan:?}") };
    assert_eq!(copy.source, key(1, "east", "east/01"));
}

#[test]
fn test_plan_fences_an_unassigned_epoch() {
    assert_eq!(
        plan(&entry(vec![source(1, "east", "east/01", 2, 128)]), 0),
        CopyPlan::Fenced
    );
}

#[test]
fn test_plan_without_a_source_has_nothing_to_copy() {
    assert_eq!(plan(&entry(Vec::new()), FENCE), CopyPlan::NoSource);
}
