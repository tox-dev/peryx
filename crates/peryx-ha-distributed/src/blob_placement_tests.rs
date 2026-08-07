use crate::blob_placement::{FetchPlan, plan_blob_fetch};
use crate::protocol::{PlacementAvailability, PlacementDescriptor};

fn placement(
    data_center: &str,
    location: &str,
    backend: &str,
    generation: u64,
    availability: PlacementAvailability,
) -> PlacementDescriptor {
    PlacementDescriptor {
        digest: "sha256:deadbeef".to_owned(),
        backend: backend.to_owned(),
        data_center: data_center.to_owned(),
        location: location.to_owned(),
        availability,
        generation,
    }
}

fn verified(data_center: &str, location: &str, backend: &str, generation: u64) -> PlacementDescriptor {
    placement(
        data_center,
        location,
        backend,
        generation,
        PlacementAvailability::Verified,
    )
}

#[test]
fn test_prefers_a_same_dc_source_over_a_higher_generation_remote() {
    let local = verified("dc-a", "vol-1", "fs", 1);
    let remote = verified("dc-b", "vol-9", "fs", 9);

    let plan = plan_blob_fetch(&[remote.clone(), local.clone()], "dc-a");

    assert_eq!(
        plan,
        FetchPlan::Sources(vec![local, remote]),
        "a same-datacenter source wins even against a newer remote one",
    );
}

#[test]
fn test_prefers_the_highest_generation_within_a_dc() {
    let old = verified("dc-a", "vol-1", "fs", 3);
    let new = verified("dc-a", "vol-2", "fs", 7);

    let plan = plan_blob_fetch(&[old.clone(), new.clone()], "dc-a");

    assert_eq!(plan, FetchPlan::Sources(vec![new, old]));
}

#[test]
fn test_a_non_verified_placement_is_excluded_so_a_verified_remote_still_wins() {
    for unusable in [
        PlacementAvailability::Pending,
        PlacementAvailability::Failed,
        PlacementAvailability::Revoked,
    ] {
        let skipped = placement("dc-a", "vol-1", "fs", 9, unusable);
        let verified_remote = verified("dc-b", "vol-2", "fs", 1);

        let plan = plan_blob_fetch(&[skipped, verified_remote.clone()], "dc-a");

        assert_eq!(
            plan,
            FetchPlan::Sources(vec![verified_remote]),
            "a same-dc high-generation {unusable:?} placement must not be a source",
        );
    }
}

#[test]
fn test_no_verified_placement_is_unavailable() {
    let plan = plan_blob_fetch(
        &[
            placement("dc-a", "vol-1", "fs", 1, PlacementAvailability::Pending),
            placement("dc-b", "vol-2", "fs", 2, PlacementAvailability::Failed),
        ],
        "dc-a",
    );

    assert_eq!(plan, FetchPlan::Unavailable);
}

#[test]
fn test_no_advertisements_is_unavailable() {
    assert_eq!(plan_blob_fetch(&[], "dc-a"), FetchPlan::Unavailable);
}

#[test]
fn test_the_tie_break_is_total_and_order_independent() {
    let by_backend = verified("dc-b", "vol-1", "s3", 5);
    let by_location = verified("dc-b", "vol-2", "fs", 5);
    let by_data_center = verified("dc-c", "vol-1", "fs", 5);
    let first = verified("dc-b", "vol-1", "fs", 5);
    let expected = FetchPlan::Sources(vec![
        first.clone(),
        by_backend.clone(),
        by_location.clone(),
        by_data_center.clone(),
    ]);

    assert_eq!(
        plan_blob_fetch(
            &[
                first.clone(),
                by_data_center.clone(),
                by_location.clone(),
                by_backend.clone()
            ],
            "dc-a",
        ),
        expected,
    );
    assert_eq!(
        plan_blob_fetch(&[by_backend, by_location, by_data_center, first], "dc-a"),
        expected,
        "the plan must not depend on the advertisements' arrival order",
    );
}
