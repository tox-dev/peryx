//! What a committed placement records, and what a failed record does to the caller.

use peryx_ha::{ArtifactPlacement, ArtifactPlacementStore as _, ArtifactSource};

use crate::meta::fault::initialized;

const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[test]
fn test_a_committed_placement_reads_local_under_its_source() {
    let (store, _inner, _fault) = initialized();

    store.record_committed_placement(DIGEST, ArtifactSource::Proxy);

    assert_eq!(
        store.get_artifact_placement(DIGEST).unwrap(),
        Some(ArtifactPlacement::record(ArtifactSource::Proxy, true))
    );
}

/// The bytes are content-addressed and already durable when this runs, so a store that cannot take
/// the row leaves the caller nothing to do about it. Every commit path depends on that: a push, a
/// pull and an import all keep their bytes when the projection write fails.
#[test]
fn test_a_failed_record_leaves_the_caller_alone() {
    let (store, _inner, fault) = initialized();
    fault.arm(0);

    store.record_committed_placement(DIGEST, ArtifactSource::Hosted);

    // Returning at all is the property: the recorder swallows the failure, so reaching this line is
    // what a push, a pull or an import relies on to keep the bytes it already committed.
    assert!(fault.triggered(), "the store write has to have failed");
}
