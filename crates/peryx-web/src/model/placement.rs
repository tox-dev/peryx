//! The artifact placement-health view models, shared by the server renderer and the hydrated client.
//!
//! The neutral [`PlacementView`] crosses the server/browser boundary unchanged; the source and byte
//! availability chips reuse the artifact page's [`file_source_label`](super::file_source_label) and
//! [`byte_availability_label`](super::byte_availability_label), so a placement row and a file row read
//! the same word for the same state.

pub use peryx_core::{
    BlobDatacenterPlacement, BlobPlacementStatus, BlobPlacementView, PlacementHealth, PlacementRow, PlacementView,
};

use super::HealthLabel;

/// A blob's per-datacenter placement status as a labelled health cell.
///
/// The word an administrator reads plus the css class that tints it. The word stands alone, so a
/// color-blind reader loses nothing, and a datacenter that does not hold a verified copy never borrows
/// the served tint.
#[must_use]
pub const fn blob_placement_status_label(status: BlobPlacementStatus) -> HealthLabel {
    match status {
        BlobPlacementStatus::Verified => HealthLabel {
            text: "Verified",
            class: "health-live",
        },
        BlobPlacementStatus::Pending => HealthLabel {
            text: "Pending",
            class: "health-unready",
        },
        BlobPlacementStatus::Failed => HealthLabel {
            text: "Failed",
            class: "health-unknown",
        },
        BlobPlacementStatus::Revoked => HealthLabel {
            text: "Revoked",
            class: "health-restricted",
        },
    }
}
