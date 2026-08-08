use crate::blob_availability::{BlobAvailability, ReferencedBlob, blob_availability};
use crate::protocol::PlacementAvailability;

fn blob(serial: u64, availability: PlacementAvailability, present_locally: bool) -> ReferencedBlob {
    ReferencedBlob {
        serial,
        digest: format!("digest-{serial}"),
        availability,
        present_locally,
        served_by_peer: false,
    }
}

/// A blob absent locally but placed on a reachable peer, so read-through serves it and it does not gate.
fn deferred(serial: u64) -> ReferencedBlob {
    ReferencedBlob {
        serial,
        digest: format!("digest-{serial}"),
        availability: PlacementAvailability::Verified,
        present_locally: false,
        served_by_peer: true,
    }
}

#[test]
fn test_all_verified_present_blobs_expose_the_whole_authority() {
    let referenced = [
        blob(1, PlacementAvailability::Verified, true),
        blob(2, PlacementAvailability::Verified, true),
    ];

    assert_eq!(
        blob_availability(2, &referenced),
        BlobAvailability {
            serial: 2,
            blocking: None,
        }
    );
}

#[test]
fn test_a_pending_blob_holds_the_frontier_below_its_serial() {
    let referenced = [
        blob(1, PlacementAvailability::Verified, true),
        blob(2, PlacementAvailability::Pending, true),
    ];

    assert_eq!(
        blob_availability(2, &referenced),
        BlobAvailability {
            serial: 1,
            blocking: Some("digest-2".to_owned()),
        }
    );
}

#[test]
fn test_a_locally_missing_blob_holds_the_frontier() {
    let referenced = [
        blob(1, PlacementAvailability::Verified, true),
        blob(2, PlacementAvailability::Verified, false),
    ];

    assert_eq!(
        blob_availability(2, &referenced),
        BlobAvailability {
            serial: 1,
            blocking: Some("digest-2".to_owned()),
        }
    );
}

#[test]
fn test_any_non_verified_placement_holds_the_frontier() {
    for availability in [
        PlacementAvailability::Pending,
        PlacementAvailability::Failed,
        PlacementAvailability::Revoked,
    ] {
        let referenced = [
            blob(1, PlacementAvailability::Verified, true),
            blob(2, availability, true),
        ];

        assert_eq!(
            blob_availability(2, &referenced),
            BlobAvailability {
                serial: 1,
                blocking: Some("digest-2".to_owned()),
            },
            "{availability:?} should hold the frontier"
        );
    }
}

#[test]
fn test_the_lowest_unavailable_serial_owns_the_frontier() {
    // The higher-serial unavailable blob sits before the lower one, so a first-encountered scan would
    // wrongly blame digest-4; only picking the minimum serial holds the frontier at digest-2.
    let referenced = [
        blob(1, PlacementAvailability::Verified, true),
        blob(4, PlacementAvailability::Pending, false),
        blob(3, PlacementAvailability::Verified, true),
        blob(2, PlacementAvailability::Failed, true),
    ];

    assert_eq!(
        blob_availability(4, &referenced),
        BlobAvailability {
            serial: 1,
            blocking: Some("digest-2".to_owned()),
        }
    );
}

#[test]
fn test_an_unordered_set_still_holds_at_the_lowest_unavailable() {
    let referenced = [
        blob(3, PlacementAvailability::Verified, true),
        blob(2, PlacementAvailability::Pending, true),
        blob(1, PlacementAvailability::Verified, true),
    ];

    assert_eq!(
        blob_availability(3, &referenced),
        BlobAvailability {
            serial: 1,
            blocking: Some("digest-2".to_owned()),
        }
    );
}

#[test]
fn test_a_blob_referenced_beyond_the_authority_does_not_gate() {
    let referenced = [
        blob(1, PlacementAvailability::Verified, true),
        blob(3, PlacementAvailability::Pending, false),
    ];

    assert_eq!(
        blob_availability(2, &referenced),
        BlobAvailability {
            serial: 2,
            blocking: None,
        }
    );
}

#[test]
fn test_an_unavailable_first_serial_holds_the_frontier_at_zero() {
    let referenced = [blob(1, PlacementAvailability::Revoked, true)];

    assert_eq!(
        blob_availability(1, &referenced),
        BlobAvailability {
            serial: 0,
            blocking: Some("digest-1".to_owned()),
        }
    );
}

#[test]
fn test_a_peer_held_blob_does_not_hold_the_frontier() {
    // The blob is absent locally but a peer datacenter holds it, so read-through serves it and the
    // frontier passes the whole authority just as a present blob would.
    let referenced = [blob(1, PlacementAvailability::Verified, true), deferred(2)];

    assert_eq!(
        blob_availability(2, &referenced),
        BlobAvailability {
            serial: 2,
            blocking: None,
        }
    );
}

#[test]
fn test_an_empty_reference_set_exposes_the_authority() {
    assert_eq!(
        blob_availability(5, &[]),
        BlobAvailability {
            serial: 5,
            blocking: None,
        }
    );
}
