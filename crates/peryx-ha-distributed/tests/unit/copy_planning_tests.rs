use std::num::NonZeroU64;
use std::str::FromStr as _;

use peryx_ha::{BackendId, BackendLocation, BlobPlacementKey, BlobPlacementRecord, BlobPlacementState, DataCenterId};
use peryx_identity::ArtifactDigest;

use super::{copy_backlog_entry, plan_cross_dc_copy};

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

fn record(
    suffix: u8,
    data_center: &str,
    location: &str,
    generation: u64,
    state: BlobPlacementState,
) -> BlobPlacementRecord {
    BlobPlacementRecord {
        key: key(suffix, data_center, location),
        state,
        fence: 1,
        transfer_attempt: 1,
        generation,
        updated_at_unix: 0,
    }
}

#[test]
fn test_backlog_selects_verified_remote_sources() {
    let entry = copy_backlog_entry(
        &[
            record(1, "east", "east/01", 2, BlobPlacementState::Verified { size: 10 }),
            record(1, "south", "south/01", 4, BlobPlacementState::Verified { size: 20 }),
        ],
        &DataCenterId::new("west").unwrap(),
        NonZeroU64::new(1).unwrap(),
    )
    .unwrap();

    let copy = plan_cross_dc_copy(
        &entry,
        &DataCenterId::new("west").unwrap(),
        &BackendId::new("filesystem").unwrap(),
        NonZeroU64::new(5).unwrap(),
    );

    assert_eq!(copy.source, key(1, "south", "south/01"));
    assert_eq!(copy.target, key(1, "west", digest(1).sha256()));
    assert_eq!(copy.size, 20);
}

#[test]
fn test_backlog_breaks_generation_ties_by_ledger_order() {
    let entry = copy_backlog_entry(
        &[
            record(1, "east", "east/01", 4, BlobPlacementState::Verified { size: 10 }),
            record(1, "south", "south/01", 4, BlobPlacementState::Verified { size: 20 }),
        ],
        &DataCenterId::new("west").unwrap(),
        NonZeroU64::new(1).unwrap(),
    )
    .unwrap();

    let copy = plan_cross_dc_copy(
        &entry,
        &DataCenterId::new("west").unwrap(),
        &BackendId::new("filesystem").unwrap(),
        NonZeroU64::new(5).unwrap(),
    );

    assert_eq!(copy.source, key(1, "east", "east/01"));
}

#[test]
fn test_backlog_skips_settled_local_placements() {
    let local = DataCenterId::new("west").unwrap();
    for state in [
        BlobPlacementState::Pending,
        BlobPlacementState::Verified { size: 10 },
        BlobPlacementState::Revoked,
    ] {
        assert!(
            copy_backlog_entry(
                &[
                    record(1, "east", "east/01", 1, BlobPlacementState::Verified { size: 10 }),
                    record(1, "west", "west/01", 1, state),
                ],
                &local,
                NonZeroU64::new(1).unwrap(),
            )
            .is_none()
        );
    }
}

#[test]
fn test_backlog_retries_failed_local_placements() {
    let entry = copy_backlog_entry(
        &[
            record(1, "east", "east/01", 1, BlobPlacementState::Verified { size: 10 }),
            record(
                1,
                "west",
                "west/01",
                1,
                BlobPlacementState::Failed {
                    class: peryx_ha::BlobPlacementFailure::SourceUnavailable,
                },
            ),
        ],
        &DataCenterId::new("west").unwrap(),
        NonZeroU64::new(1).unwrap(),
    );

    assert!(entry.is_some());
}

#[test]
fn test_backlog_retries_pending_after_an_ownership_transition() {
    let entry = copy_backlog_entry(
        &[
            record(1, "east", "east/01", 1, BlobPlacementState::Verified { size: 10 }),
            record(1, "west", "west/01", 1, BlobPlacementState::Pending),
        ],
        &DataCenterId::new("west").unwrap(),
        NonZeroU64::new(2).unwrap(),
    );

    assert!(entry.is_some());
}

#[test]
fn test_backlog_requires_records_and_a_verified_source() {
    let local = DataCenterId::new("west").unwrap();

    assert!(copy_backlog_entry(&[], &local, NonZeroU64::new(1).unwrap()).is_none());
    assert!(
        copy_backlog_entry(
            &[record(1, "east", "east/01", 1, BlobPlacementState::Pending)],
            &local,
            NonZeroU64::new(1).unwrap(),
        )
        .is_none()
    );
}
