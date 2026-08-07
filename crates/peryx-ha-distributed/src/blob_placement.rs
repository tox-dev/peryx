//! Selecting which peer placement to fetch a blob from, and in what order.
//!
//! The dual-plane blob replication lets a DC without local bytes fetch a blob from a peer that holds it.
//! Peers advertise their placements as [`PlacementDescriptor`]s, and a fetch has to choose among them:
//! prefer a copy in the same datacenter so the fetch stays on the local network, then the newest
//! generation, and skip any placement that cannot serve the bytes. This is the pure selection: it reads
//! the advertisements and the local datacenter and returns an ordered fetch plan, or reports that no
//! placement can serve the blob.
//!
//! Only a [`Verified`](PlacementAvailability::Verified) placement is a real source. A
//! [`Pending`](PlacementAvailability::Pending) peer has not confirmed it holds the blob, so fetching it
//! wastes a round-trip that likely answers not-found; a [`Failed`](PlacementAvailability::Failed) copy is
//! dead; and a [`Revoked`](PlacementAvailability::Revoked) one is not a source. When nothing is verified
//! the plan is [`Unavailable`](FetchPlan::Unavailable), and the caller retries once a source turns
//! verified rather than fetching a placement that cannot answer.
//!
//! This is the pure core. The driver-side range forwarding, streaming verification, source retry, and
//! circuit breaking that consume a plan are deferred wiring.

use std::cmp::Ordering;

use crate::protocol::{PlacementAvailability, PlacementDescriptor};

/// The ordered sources a blob fetch should try, or that no placement can serve it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchPlan {
    /// The verified placements to fetch from, best first: same datacenter before remote, then the
    /// highest generation, then a stable order so the plan is deterministic.
    Sources(Vec<PlacementDescriptor>),
    /// No advertised placement is verified, so the blob cannot be fetched until one becomes a source.
    Unavailable,
}

/// Plan a blob fetch from the peer `placements` advertised for it, preferring a source in `local_dc`.
///
/// Only [`Verified`](PlacementAvailability::Verified) placements are sources; the rest are dropped. The
/// survivors are ordered same-datacenter first, then by descending generation, then lexicographically by
/// datacenter, location, and backend so two runs over the same advertisements always plan the same
/// order. With no verified placement the result is [`FetchPlan::Unavailable`].
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

/// The total order a fetch plan sorts by: a same-datacenter source outranks a remote one, a higher
/// generation outranks a lower, and datacenter, location, then backend break any remaining tie so the
/// order never depends on the advertisements' arrival order.
fn fetch_order(a: &PlacementDescriptor, b: &PlacementDescriptor, local_dc: &str) -> Ordering {
    let remote = |placement: &PlacementDescriptor| placement.data_center != local_dc;
    remote(a)
        .cmp(&remote(b))
        .then_with(|| b.generation.cmp(&a.generation))
        .then_with(|| a.data_center.cmp(&b.data_center))
        .then_with(|| a.location.cmp(&b.location))
        .then_with(|| a.backend.cmp(&b.backend))
}
