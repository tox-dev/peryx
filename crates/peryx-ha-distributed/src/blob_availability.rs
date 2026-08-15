//! Metadata becomes serveable only when each referenced blob has a verified placement and either local
//! verified bytes or a reachable read-through peer. The frontier stops below the first unavailable blob.

use crate::protocol::PlacementAvailability;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferencedBlob {
    pub serial: u64,
    pub digest: String,
    pub availability: PlacementAvailability,
    pub present_locally: bool,
    /// A reachable peer can serve the blob through cross-DC read-through despite its local absence.
    pub served_by_peer: bool,
}

impl ReferencedBlob {
    #[must_use]
    pub const fn is_available(&self) -> bool {
        (self.present_locally || self.served_by_peer) && matches!(self.availability, PlacementAvailability::Verified)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BlobAvailability {
    /// Each referenced blob at or below this serial is serveable.
    pub serial: u64,
    /// The first unavailable digest, or `None` when the result reaches the authority.
    pub blocking: Option<String>,
}

/// Returns `authority` or the serial before its lowest unavailable reference. References may be unordered;
/// entries above `authority` do not gate it, and an empty set returns `authority`.
#[must_use]
pub fn blob_availability(authority: u64, referenced: &[ReferencedBlob]) -> BlobAvailability {
    let lowest_unavailable = referenced
        .iter()
        .filter(|blob| blob.serial <= authority && !blob.is_available())
        .min_by_key(|blob| blob.serial);
    lowest_unavailable.map_or(
        BlobAvailability {
            serial: authority,
            blocking: None,
        },
        |blob| BlobAvailability {
            serial: blob.serial.saturating_sub(1),
            blocking: Some(blob.digest.clone()),
        },
    )
}
