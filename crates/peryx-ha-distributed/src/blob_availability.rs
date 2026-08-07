//! The blob-availability frontier: how far a replica's metadata is backed by blobs it can actually serve.
//!
//! A metadata change may reference content blobs, and a replica must not expose that metadata until the
//! blobs behind it are present and serveable — otherwise a reader follows a fresh record to a byte range
//! the replica cannot answer. This folds the placements a peer advertises and this replica's local
//! presence into the highest serial through which every referenced blob is available, the blob analog of
//! the [readable frontier](peryx-driver derived views) that gates on rebuilt derived views. The replica
//! loop feeds this serial into that gate alongside the view frontiers; this module only reports the
//! serial, it never gates a read itself.
//!
//! The fold is pure and total: placements and local presence are inputs, never fetched, so the result is
//! decided without any transport, storage, clock, or replica loop.
//!
//! A blob is available only when this replica holds verified bytes for it AND the authority advertises the
//! placement as [`Verified`](crate::protocol::PlacementAvailability::Verified). A blob still
//! [`Pending`](crate::protocol::PlacementAvailability::Pending), one whose placement
//! [`Failed`](crate::protocol::PlacementAvailability::Failed) or was
//! [`Revoked`](crate::protocol::PlacementAvailability::Revoked), and one whose bytes are simply not here
//! yet are all unavailable — the frontier fails closed and holds below the first serial that references
//! one, so a revoked or half-uploaded blob is never exposed behind fresh metadata.

use crate::protocol::PlacementAvailability;

/// One blob a metadata serial references, with the evidence deciding whether this replica can serve it.
///
/// The evidence is the placement the authority advertises, whether the bytes are present and verified
/// locally, and whether a peer datacenter holds them for cross-DC read-through to serve on demand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferencedBlob {
    /// The metadata serial that references the blob. A digest referenced by several serials appears once
    /// per serial, so the lowest one is where an unavailable blob first holds the frontier.
    pub serial: u64,
    /// The blob's content digest, reported as the blocker when it is what holds the frontier back.
    pub digest: String,
    /// The placement lifecycle the authority advertises for the blob.
    pub availability: PlacementAvailability,
    /// Whether this replica holds verified bytes for the blob.
    pub present_locally: bool,
    /// Whether the policy places the blob only on a reachable peer datacenter, so this replica defers it
    /// to cross-DC read-through rather than holding the bytes. A deferred blob is serveable through the
    /// peer that holds it, so it does not hold the frontier back the way a plain local miss does.
    pub served_by_peer: bool,
}

impl ReferencedBlob {
    /// Whether this replica can serve the blob: the authority advertises the placement as
    /// [`Verified`](PlacementAvailability::Verified), and either verified bytes are present locally or a
    /// peer datacenter holds them for read-through to serve.
    #[must_use]
    pub const fn is_available(&self) -> bool {
        (self.present_locally || self.served_by_peer) && matches!(self.availability, PlacementAvailability::Verified)
    }
}

/// The highest metadata serial whose referenced blobs a replica can all serve, and the blob holding it
/// back.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BlobAvailability {
    /// The serial safe to serve for blob backing: every blob referenced at or below it is available.
    pub serial: u64,
    /// The digest of the blob that pins `serial` below the authoritative frontier, or `None` when every
    /// referenced blob up to the authority is available.
    pub blocking: Option<String>,
}

/// The blob-availability frontier for an `authority` serial given the blobs its changes `referenced`.
///
/// The frontier is the authoritative serial, pulled back to just below the lowest serial that references
/// an unavailable blob: every referenced blob at or below the result is serveable, and the change that
/// first references an unavailable one is held out along with everything after it. Blobs referenced beyond
/// `authority` are past the metadata frontier and gate nothing, so they are ignored. `referenced` need not
/// be sorted; the lowest unavailable serial wins regardless of order.
///
/// An empty set — no change in range references a blob — imposes no blob constraint, so the frontier is
/// the whole `authority` serial with no blocker. This is the defined floor for the unconstrained case, not
/// a fail-open guess: with nothing to wait on, blobs hold back nothing.
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
