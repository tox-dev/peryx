use std::collections::BTreeSet;
use std::str::FromStr as _;

use peryx_ha::{BackendId, BackendLocation, BlobPlacementKey, BlobPlacementRecord, BlobPlacementState, DataCenterId};
use peryx_identity::ArtifactDigest;

use super::out_of_policy_placements;

fn record(data_center: &str, location: &str, state: BlobPlacementState) -> BlobPlacementRecord {
    BlobPlacementRecord {
        key: BlobPlacementKey {
            digest: ArtifactDigest::from_str(&format!("sha256:{:064x}", 1)).unwrap(),
            backend: BackendId::new("filesystem").unwrap(),
            data_center: DataCenterId::new(data_center).unwrap(),
            location: BackendLocation::new(location).unwrap(),
        },
        state,
        fence: 1,
        generation: 1,
        updated_at_unix: 0,
    }
}

#[test]
fn test_only_verified_placements_outside_policy_retire() {
    let east = record("east", "east/01", BlobPlacementState::Verified { size: 1 });
    let west = record("west", "west/01", BlobPlacementState::Verified { size: 1 });
    let pending = record("south", "south/01", BlobPlacementState::Pending);
    let target = BTreeSet::from([DataCenterId::new("west").unwrap()]);

    assert_eq!(
        out_of_policy_placements(&[east.clone(), west, pending], &target),
        vec![east.key]
    );
}
