use std::collections::BTreeSet;

use peryx_ha::{BlobPlacementKey, BlobPlacementRecord, BlobPlacementState, DataCenterId};

pub fn out_of_policy_placements(
    records: &[BlobPlacementRecord],
    target_dcs: &BTreeSet<DataCenterId>,
) -> Vec<BlobPlacementKey> {
    records
        .iter()
        .filter(|record| {
            matches!(record.state, BlobPlacementState::Verified { .. }) && !target_dcs.contains(&record.key.data_center)
        })
        .map(|record| record.key.clone())
        .collect()
}

#[cfg(test)]
#[path = "../tests/unit/placement_planning_tests.rs"]
mod tests;
