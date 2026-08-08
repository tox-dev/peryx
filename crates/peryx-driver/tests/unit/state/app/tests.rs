use peryx_core::{NodeRole, TopologyConfig, TopologyMember, TopologyMode};
use peryx_identity::ArtifactDigest;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::{
    BackendLocation, BlobPlacementKey, BlobPlacementState, BlobPlacementTransition, DataCenterId, MetaStore,
};

use super::{AppState, ServingState};

/// A well-formed lowercase-hex sha256 (`abcd` sixteen times) the wrapper accepts as an artifact digest.
const DIGEST_HEX: &str = "abcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcd";

fn member(node: &str, dc: &str) -> TopologyMember {
    TopologyMember {
        node: node.to_owned(),
        dc: dc.to_owned(),
        address: format!("{node}:8080"),
        role: NodeRole::Writer,
    }
}

fn home_topology(dc: &str) -> TopologyConfig {
    TopologyConfig {
        mode: TopologyMode::Dc,
        group: Some("group".to_owned()),
        members: vec![member("writer", dc)],
        local_node: Some("writer".to_owned()),
    }
}

fn state_with(topology: TopologyConfig) -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::new(meta, blobs, 60, Vec::new());
    state.set_availability_topology(topology);
    (dir, state)
}

fn home_key(state: &ServingState, digest: &ArtifactDigest, dc: &str) -> BlobPlacementKey {
    BlobPlacementKey {
        digest: digest.clone(),
        backend: state.blobs.backend_id(),
        data_center: DataCenterId::new(dc).unwrap(),
        location: BackendLocation::for_digest(digest),
    }
}

#[test]
fn test_record_home_placement_verifies_the_home_datacenter() {
    let (_dir, state) = state_with(home_topology("home"));
    state.record_home_placement(DIGEST_HEX, 2_048, 3);
    let digest = ArtifactDigest::from_sha256(DIGEST_HEX).unwrap();
    let record = state
        .meta
        .blob_placement(&home_key(&state, &digest, "home"))
        .unwrap()
        .unwrap();
    assert_eq!(record.state, BlobPlacementState::Verified { size: 2_048 });
}

#[test]
fn test_record_home_placement_skips_a_node_without_a_local_datacenter() {
    let (_dir, state) = state_with(TopologyConfig::default());
    state.record_home_placement(DIGEST_HEX, 2_048, 3);
    let digest = ArtifactDigest::from_sha256(DIGEST_HEX).unwrap();
    assert!(
        state.meta.blob_placements(&digest).unwrap().is_empty(),
        "a node that resolves no local datacenter records nothing",
    );
}

#[test]
fn test_record_home_placement_swallows_a_malformed_digest() {
    let (_dir, state) = state_with(home_topology("home"));
    state.record_home_placement("not-a-sha256", 2_048, 3);
    let digest = ArtifactDigest::from_sha256(DIGEST_HEX).unwrap();
    assert!(
        state.meta.blob_placements(&digest).unwrap().is_empty(),
        "a malformed digest is swallowed and records nothing",
    );
}

#[test]
fn test_record_home_placement_swallows_an_invalid_datacenter_component() {
    let (_dir, state) = state_with(home_topology("bad\0dc"));
    state.record_home_placement(DIGEST_HEX, 2_048, 3);
    let digest = ArtifactDigest::from_sha256(DIGEST_HEX).unwrap();
    assert!(
        state.meta.blob_placements(&digest).unwrap().is_empty(),
        "a datacenter label the placement key rejects is swallowed",
    );
}

#[test]
fn test_record_home_placement_swallows_a_stale_fence() {
    let (_dir, state) = state_with(home_topology("home"));
    let digest = ArtifactDigest::from_sha256(DIGEST_HEX).unwrap();
    let key = home_key(&state, &digest, "home");
    state
        .meta
        .apply_blob_placement(&key, &BlobPlacementTransition::Stage, 5, 10)
        .unwrap();

    state.record_home_placement(DIGEST_HEX, 2_048, 2);

    let record = state.meta.blob_placement(&key).unwrap().unwrap();
    assert_eq!(
        record.state,
        BlobPlacementState::Pending,
        "a stale-fenced write is swallowed and leaves the staged record unchanged",
    );
}
