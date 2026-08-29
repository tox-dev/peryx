use std::str::FromStr as _;

use peryx_identity::ArtifactDigest;
use peryx_storage::meta::{
    BackendId, BackendLocation, BlobPlacementFailure, BlobPlacementKey, BlobPlacementRecord, BlobPlacementState,
    DataCenterId,
};

use crate::{PlacementAvailability, PlacementDescriptor};

fn record(state: BlobPlacementState, generation: u64) -> BlobPlacementRecord {
    BlobPlacementRecord {
        key: BlobPlacementKey {
            digest: ArtifactDigest::from_str(&format!("sha256:{:064x}", 1)).unwrap(),
            backend: BackendId::new("s3").unwrap(),
            data_center: DataCenterId::new("dc-a").unwrap(),
            location: BackendLocation::new("blobs/aa").unwrap(),
        },
        state,
        fence: 3,
        transfer_attempt: 1,
        generation,
        updated_at_unix: 100,
    }
}

#[test]
fn test_descriptor_carries_the_key_and_generation() {
    let source = record(BlobPlacementState::Verified { size: 9 }, 4);
    let descriptor = PlacementDescriptor::from(&source);
    assert_eq!(descriptor.digest, source.key.digest.canonical());
    assert_eq!(descriptor.backend, "s3");
    assert_eq!(descriptor.data_center, "dc-a");
    assert_eq!(descriptor.location, "blobs/aa");
    assert_eq!(descriptor.availability, PlacementAvailability::Verified);
    assert_eq!(descriptor.generation, 4);
}

#[test]
fn test_availability_maps_and_round_trips_each_status() {
    for (state, expected, wire) in [
        (BlobPlacementState::Pending, PlacementAvailability::Pending, "pending"),
        (
            BlobPlacementState::Verified { size: 1 },
            PlacementAvailability::Verified,
            "verified",
        ),
        (
            BlobPlacementState::Failed {
                class: BlobPlacementFailure::DigestMismatch,
            },
            PlacementAvailability::Failed,
            "failed",
        ),
        (BlobPlacementState::Revoked, PlacementAvailability::Revoked, "revoked"),
    ] {
        let descriptor = PlacementDescriptor::from(&record(state, 1));
        assert_eq!(descriptor.availability, expected);
        let encoded = serde_json::to_value(&descriptor).unwrap();
        assert_eq!(encoded["availability"], wire);
        assert_eq!(
            serde_json::from_value::<PlacementDescriptor>(encoded).unwrap(),
            descriptor
        );
    }
}
