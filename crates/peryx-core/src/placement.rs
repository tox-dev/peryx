//! The neutral artifact-placement-health view an operator surface renders.
//!
//! An administrator needs to see how much of the store serves locally, how much depends on an upstream,
//! and how much cannot be served at all, without paging every artifact by hand or reading tenant data
//! from a topology view. [`PlacementView`] carries the aggregate every viewer reads and, for an
//! administrator, a bounded page of per-digest rows. The models are pure serde with no I/O, so the same
//! type crosses the server/browser boundary the topology view already established.
//!
//! The aggregate is whole-store; the rows are one capped page in digest order with a cursor to resume,
//! so a large store never turns a render into an unbounded payload.

use serde::{Deserialize, Serialize};

use crate::view::{UiArtifactSource, UiByteAvailability};

/// How the store's byte availability splits across its artifacts, plus the total it sums to. Three
/// counts and a total regardless of store size, so the summary never scales with the object count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PlacementHealth {
    /// Artifacts this instance serves from local storage.
    pub local: u64,
    /// Artifacts with no local bytes that a known upstream can still supply.
    pub remote_only: u64,
    /// Artifacts with no local bytes and no upstream to supply them.
    pub unavailable: u64,
    /// The sum of the three states, so a reader sees the store size the split covers.
    pub total: u64,
}

/// One artifact's placement: its content digest and the two orthogonal dimensions a health view reads.
///
/// Carries no file path, tenant identity, or repository coordinate, so an administrator inspects
/// convergence by digest without the row leaking where the artifact lives or who owns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementRow {
    pub digest: String,
    pub source: UiArtifactSource,
    pub availability: UiByteAvailability,
}

/// The placement-health view filtered to the caller's class.
///
/// Every admitted caller reads the aggregate and the observation time; only an administrator reads the
/// per-digest `rows` and the `next_cursor` to page them. A withheld page is absent rather than empty, so
/// an operator cannot mistake a filtered view for a converged store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementView {
    /// Unix seconds when the view was taken, so a stale render shows as age rather than health.
    pub captured_at: i64,
    pub health: PlacementHealth,
    /// A bounded page of per-digest rows in digest order, present only for an administrator.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<Vec<PlacementRow>>,
    /// The digest to resume the page after, present only with `rows` and only when more remain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// The lifecycle of one datacenter's copy of a blob, mirroring the storage placement state without its
/// internal evidence payload. `Verified` is the only state that serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlobPlacementStatus {
    /// A transfer into the datacenter is in flight or staged; no serving copy exists there yet.
    Pending,
    /// The datacenter holds a verified copy it can serve.
    Verified,
    /// A transfer into the datacenter failed; it holds no serving copy.
    Failed,
    /// The datacenter's copy was withdrawn from serving.
    Revoked,
}

/// One datacenter's copy of a blob: which datacenter holds it and in what state, plus the verified byte
/// size and when the record last moved.
///
/// Names the datacenter alone, never the backend identity or the on-disk location, so an administrator
/// reads convergence across datacenters without a row leaking an internal path or backend name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobDatacenterPlacement {
    pub data_center: String,
    pub status: BlobPlacementStatus,
    /// The verified byte size, present only once a datacenter holds a verified copy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Unix seconds when this datacenter's record last changed, so a stale copy shows as age.
    pub updated_at: i64,
}

/// Where one blob's bytes are placed across datacenters, keyed by its content digest.
///
/// The administrator-only companion to the whole-store [`PlacementView`]: it answers which datacenters
/// hold a given blob and in what state, in datacenter order, capped by the store's per-digest placement
/// bound so one digest cannot return an unbounded list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobPlacementView {
    pub digest: String,
    pub datacenters: Vec<BlobDatacenterPlacement>,
}

#[cfg(test)]
#[path = "../tests/unit/placement/tests.rs"]
mod tests;
