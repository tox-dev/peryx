//! Fetch plans exclude unverified placements, prefer the local datacenter, and then prefer the newest
//! generation. Stable tie-breakers keep plans independent of advertisement arrival order.

use std::cmp::Ordering;

use crate::protocol::{PlacementAvailability, PlacementDescriptor};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchPlan {
    Sources(Vec<PlacementDescriptor>),
    Unavailable,
}

/// Orders verified sources by locality, generation, and stable placement keys.
///
/// Returns [`FetchPlan::Unavailable`] when no verified source exists.
#[must_use]
pub fn plan_blob_fetch(placements: &[PlacementDescriptor], local_dc: &str) -> FetchPlan {
    let mut sources: Vec<PlacementDescriptor> = placements
        .iter()
        .filter(|placement| placement.availability == PlacementAvailability::Verified)
        .cloned()
        .collect();
    if sources.is_empty() {
        return FetchPlan::Unavailable;
    }
    sources.sort_by(|a, b| fetch_order(a, b, local_dc));
    FetchPlan::Sources(sources)
}

fn fetch_order(a: &PlacementDescriptor, b: &PlacementDescriptor, local_dc: &str) -> Ordering {
    let remote = |placement: &PlacementDescriptor| placement.data_center != local_dc;
    remote(a)
        .cmp(&remote(b))
        .then_with(|| b.generation.cmp(&a.generation))
        .then_with(|| a.data_center.cmp(&b.data_center))
        .then_with(|| a.location.cmp(&b.location))
        .then_with(|| a.backend.cmp(&b.backend))
}
