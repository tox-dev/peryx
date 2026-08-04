//! Selecting which datacenter targets a verified filesystem artifact must be copied to.
//!
//! Local-filesystem HA keeps a configured set of datacenters each holding a copy of an artifact. A
//! blob's advertised placements say which datacenters already hold it and at what generation; the
//! background copier reads those against the configured targets and plans the copies still owed. This
//! is the pure selection: it reads the advertisements, the configured targets, and the committed
//! placement generation, and returns the targets to copy to, that no source can serve the copy, or that
//! every target already holds it.
//!
//! Generation fences the plan. Metadata advances a blob's placement generation when it supersedes the
//! bytes, so a placement below the committed generation is stale: it counts neither as a source to copy
//! from nor as a target that already holds the current bytes, and a target whose only copy is stale is
//! still owed a fresh one. Only a [`Verified`](PlacementAvailability::Verified) placement at the
//! committed generation is a real source or a satisfied target.
//!
//! This is the pure core. Streaming the bytes over a blob transport, staging and atomically publishing
//! them with a placement receipt, bounding concurrency and bandwidth, and retrying under the worker
//! runtime are the copier's deferred wiring.

use std::collections::BTreeSet;

use crate::protocol::{PlacementAvailability, PlacementDescriptor};

/// The datacenter copies a background transfer should make, or why it makes none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyPlan {
    /// The target datacenters still owed a copy, deduplicated and in a deterministic order.
    Targets(Vec<String>),
    /// No placement is a verified source at the committed generation, so the copy waits for one.
    NoSource,
    /// Every configured target already holds a verified copy at the committed generation.
    Complete,
}

/// Plan the background datacenter copies for a blob from its advertised `placements` to `targets`,
/// fenced to `committed_generation`.
///
/// Only a [`Verified`](PlacementAvailability::Verified) placement at `committed_generation` counts: it
/// is a source to copy from and, for a target datacenter, proof the target already holds the current
/// bytes. With no such source the plan is [`CopyPlan::NoSource`]. Otherwise the plan is the configured
/// targets that hold no current verified copy, deduplicated and ordered so two runs over the same
/// advertisements plan the same order; when none remain the plan is [`CopyPlan::Complete`].
#[must_use]
pub fn plan_dc_copy(placements: &[PlacementDescriptor], targets: &[String], committed_generation: u64) -> CopyPlan {
    let is_current_source = |placement: &PlacementDescriptor| {
        placement.generation == committed_generation && placement.availability == PlacementAvailability::Verified
    };
    if !placements.iter().any(is_current_source) {
        return CopyPlan::NoSource;
    }
    let held: BTreeSet<&str> = placements
        .iter()
        .filter(|&placement| is_current_source(placement))
        .map(|placement| placement.data_center.as_str())
        .collect();
    let owed: BTreeSet<String> = targets
        .iter()
        .filter(|target| !held.contains(target.as_str()))
        .cloned()
        .collect();
    if owed.is_empty() {
        CopyPlan::Complete
    } else {
        CopyPlan::Targets(owed.into_iter().collect())
    }
}
