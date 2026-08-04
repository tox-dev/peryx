use crate::dc_copy::{CopyPlan, plan_dc_copy};
use crate::protocol::{PlacementAvailability, PlacementDescriptor};

fn placement(data_center: &str, availability: PlacementAvailability, generation: u64) -> PlacementDescriptor {
    PlacementDescriptor {
        digest: "sha256:deadbeef".to_owned(),
        backend: "fs".to_owned(),
        data_center: data_center.to_owned(),
        location: format!("/blobs/{data_center}"),
        availability,
        generation,
    }
}

fn targets(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

#[test]
fn test_a_pending_placement_is_not_a_source() {
    let placements = [placement("dc-a", PlacementAvailability::Pending, 1)];
    assert_eq!(plan_dc_copy(&placements, &targets(&["dc-b"]), 1), CopyPlan::NoSource);
}

#[test]
fn test_a_stale_generation_placement_is_not_a_source() {
    let placements = [placement("dc-a", PlacementAvailability::Verified, 1)];
    assert_eq!(plan_dc_copy(&placements, &targets(&["dc-b"]), 2), CopyPlan::NoSource);
}

#[test]
fn test_a_failed_placement_is_not_a_source() {
    let placements = [placement("dc-a", PlacementAvailability::Failed, 2)];
    assert_eq!(plan_dc_copy(&placements, &targets(&["dc-b"]), 2), CopyPlan::NoSource);
}

#[test]
fn test_a_target_without_a_current_copy_is_owed() {
    let placements = [placement("dc-a", PlacementAvailability::Verified, 2)];
    assert_eq!(
        plan_dc_copy(&placements, &targets(&["dc-a", "dc-b"]), 2),
        CopyPlan::Targets(vec!["dc-b".to_owned()]),
        "the source datacenter is satisfied while the other is owed a copy"
    );
}

#[test]
fn test_a_target_holding_only_a_stale_copy_is_still_owed() {
    let placements = [
        placement("dc-a", PlacementAvailability::Verified, 2),
        placement("dc-b", PlacementAvailability::Verified, 1),
    ];
    assert_eq!(
        plan_dc_copy(&placements, &targets(&["dc-b"]), 2),
        CopyPlan::Targets(vec!["dc-b".to_owned()]),
        "a target whose only verified copy is a stale generation still needs the current bytes"
    );
}

#[test]
fn test_every_target_holding_a_current_copy_is_complete() {
    let placements = [
        placement("dc-a", PlacementAvailability::Verified, 2),
        placement("dc-b", PlacementAvailability::Verified, 2),
    ];
    assert_eq!(
        plan_dc_copy(&placements, &targets(&["dc-a", "dc-b"]), 2),
        CopyPlan::Complete
    );
}

#[test]
fn test_owed_targets_are_deduplicated_and_ordered() {
    let placements = [placement("dc-x", PlacementAvailability::Verified, 1)];
    assert_eq!(
        plan_dc_copy(&placements, &targets(&["dc-c", "dc-a", "dc-c", "dc-b"]), 1),
        CopyPlan::Targets(vec!["dc-a".to_owned(), "dc-b".to_owned(), "dc-c".to_owned()]),
        "a repeated or unsorted target list plans one stable, deduplicated order"
    );
}
