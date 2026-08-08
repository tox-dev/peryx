//! OCI mutations through a home-datacenter failover, over a live multi-process `ha` group.
//!
//! These scenarios drive real OCI distribution-spec mutating operations against a spawned `ha` cluster and
//! assert an OCI client observes one coherent terminal result across a home-datacenter failure: a blob push
//! and a manifest publication to the home writer commit, and their bytes, digests, and the tag that names
//! the manifest survive the home datacenter dying and recovering; a retry after recovery resolves to the
//! same content-addressed blobs and the same single tag rather than a duplicate; and a read-only replica
//! refuses every OCI mutation before and after the home fails, so no push is ever silently accepted off the
//! home authority.
//!
//! They observe only the public `/v2/` HTTP surface and the admin-gated availability resources, the way a
//! registry client and an operator would, and they wait on asserted state (readiness, a served blob, an
//! agreed leader) rather than sleeping a fixed span, so the outcome is deterministic. An OCI hosted write
//! commits home-locally and replicates asynchronously in this harness - no remote datacenter acknowledges
//! its cross-DC durability - so the terminal contract under test is that a committed blob, manifest, and tag
//! stay home-durable and idempotent across the failover, not that the write reaches a cross-DC-durable ack.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::harness::{
    ADMIN_PASSWORD, ADMIN_USER, Cluster, MemberSpec, Node, OCI_MANIFEST_TYPE, Role, Topology, oci_digest,
};

/// The read-only replica's flat rejection of any mutation, the neutral peryx envelope returned by the
/// availability middleware before routing reaches the OCI driver, so a blob push, a manifest publish, and a
/// manifest delete all fail identically off the home authority.
const READ_ONLY_BODY: &str = r#"{"error":"read_only_replica","message":"this replica does not accept mutations"}"#;

/// The image repository the scenarios push to, under the hosted OCI index at `OCI_ROUTE`.
const REPO: &str = "app";

/// The image config blob a manifest names. Its bytes are content-addressed, so re-pushing them is
/// idempotent.
const CONFIG: &[u8] = br#"{"architecture":"amd64","os":"linux"}"#;

/// The one image layer the manifest lists.
const LAYER: &[u8] = b"a-single-image-layer-of-bytes";

/// A home-datacenter `ha` group serving an OCI index: one writer at `east` and a read replica in each of two
/// other datacenters, so a quorum of two survives the writer's death and the group keeps a committed leader
/// through the failover.
fn home_oci_group() -> Cluster {
    Topology::ha(
        "global",
        vec![
            MemberSpec::new("writer-east", "east", Role::Writer),
            MemberSpec::new("replica-west", "west", Role::Replica),
            MemberSpec::new("replica-south", "south", Role::Replica),
        ],
    )
    .with_admin()
    .with_oci()
    .with_write_ack_deadline(1)
    .start()
    .expect("the ha group starts")
}

/// An OCI image manifest naming the config and one layer by their content digests, byte-stable so its own
/// digest is deterministic. Both referenced blobs must already be members of the repository, or the publish
/// is rejected with `MANIFEST_BLOB_UNKNOWN`.
fn image_manifest() -> Vec<u8> {
    format!(
        concat!(
            r#"{{"schemaVersion":2,"mediaType":"{manifest}","#,
            r#""config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{config}","size":{config_size}}},"#,
            r#""layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"{layer}","size":{layer_size}}}]}}"#,
        ),
        manifest = OCI_MANIFEST_TYPE,
        config = oci_digest(CONFIG),
        config_size = CONFIG.len(),
        layer = oci_digest(LAYER),
        layer_size = LAYER.len(),
    )
    .into_bytes()
}

/// Push the config blob, the layer blob, then the manifest under tag `1.0`, asserting each commits, and
/// return the manifest's digest. Content-addressed and idempotent, so a retry drives the identical calls.
fn push_image(node: &Node) -> String {
    for blob in [CONFIG, LAYER] {
        let (code, _) = node.oci_push_blob(REPO, blob).expect("blob push reaches the writer");
        assert!(
            matches!(code, 201 | 202),
            "a home blob push commits home-locally: {code}",
        );
    }
    let manifest = image_manifest();
    let (code, digest) = node
        .oci_put_manifest(REPO, "1.0", &manifest, OCI_MANIFEST_TYPE)
        .expect("manifest publish reaches the writer");
    assert!(
        matches!(code, 201 | 202),
        "a home manifest publish commits home-locally: {code}",
    );
    digest
}

/// Assert the node holds the pushed image intact: both blobs pull back byte for byte, tag `1.0` resolves to
/// `manifest_digest` and returns the manifest bytes, and the repository lists that one tag with no
/// duplicate.
fn assert_image_intact(node: &Node, manifest_digest: &str) {
    for blob in [CONFIG, LAYER] {
        let (code, bytes) = node
            .oci_pull_blob(REPO, &oci_digest(blob))
            .expect("blob pull reaches the node");
        assert_eq!((code, bytes.as_slice()), (200, blob), "the node serves the pushed blob");
    }
    let (code, resolved, bytes) = node
        .oci_get_manifest(REPO, "1.0")
        .expect("manifest resolution reaches the node");
    assert_eq!(code, 200, "the tag still resolves after recovery");
    assert_eq!(
        resolved.as_deref(),
        Some(manifest_digest),
        "the tag resolves to the same manifest digest",
    );
    assert_eq!(bytes, image_manifest(), "the resolved manifest is the published bytes");
    assert_eq!(
        node.oci_tags(REPO),
        vec!["1.0".to_owned()],
        "the repository lists the one tag once"
    );
}

/// Wait until a quorum of nodes agrees on the same committed leader over three voters, the signal that the
/// `ha` group has formed and can drive its durability plane. Polls state rather than sleeping a fixed span.
fn await_group_leader(cluster: &Cluster) -> String {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Some(leader) = quorum_leader(cluster) {
            return leader;
        }
        assert!(
            Instant::now() < deadline,
            "the ha group did not agree on a leader:\n{}",
            cluster.failure_report().render(),
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Wait until a quorum agrees on a leader other than `old`, the signal that authority failed over off the
/// killed home datacenter.
fn await_leader_change(cluster: &Cluster, old: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Some(leader) = quorum_leader(cluster).filter(|leader| leader != old) {
            return leader;
        }
        assert!(
            Instant::now() < deadline,
            "authority did not leave {old}:\n{}",
            cluster.failure_report().render(),
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The leader a majority of nodes report under a committed three-voter membership, or `None` until they
/// concur. A single node's view lags the group by a heartbeat, so agreement across a quorum is the settled
/// signal that never flaps on a transient read.
fn quorum_leader(cluster: &Cluster) -> Option<String> {
    let mut tally: HashMap<String, usize> = HashMap::new();
    for node in cluster.nodes() {
        let Some((200, body)) = node.control_get_as(ADMIN_USER, ADMIN_PASSWORD, "/availability/v1/status") else {
            continue;
        };
        let Ok(status) = serde_json::from_str::<Value>(&body) else {
            continue;
        };
        let Some(consensus) = status.get("consensus") else {
            continue;
        };
        let three_voters = consensus
            .get("voters")
            .and_then(Value::as_array)
            .is_some_and(|voters| voters.len() == 3);
        if let Some(leader) = consensus.get("leader").and_then(Value::as_str)
            && three_voters
        {
            *tally.entry(leader.to_owned()).or_default() += 1;
        }
    }
    tally
        .into_iter()
        .find(|(_, count)| *count >= 2)
        .map(|(leader, _)| leader)
}

/// Wait until the node serves the config blob with exactly its bytes, the signal that the home store came
/// back intact after a restart. Polls the served artifact, so a slow reopen waits rather than racing.
fn await_image_served(node: &Node) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some((200, bytes)) = node.oci_pull_blob(REPO, &oci_digest(CONFIG))
            && bytes == CONFIG
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the home node did not serve the image after recovery: {:?}",
            node.status(),
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Wait until the node stops answering HTTP, so a test acts on the home's death rather than racing the kill.
fn await_down(node: &Node) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while node.status().is_some() {
        assert!(Instant::now() < deadline, "the killed node kept answering HTTP");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn test_home_dc_oci_push_is_retry_safe_and_survives_a_home_failure() {
    // Push an image (config blob, layer blob, tagged manifest) to the home writer, then kill and restart the
    // home datacenter and retry the identical push. The client must observe one terminal result across the
    // failure: the blobs' bytes and digests, the manifest bytes and digest, and the tag that names it all
    // survive the home dying, and the retry resolves to the same content-addressed image rather than a
    // duplicate tag or a second manifest.
    let mut cluster = home_oci_group();
    await_group_leader(&cluster);
    let writer = cluster.node("writer-east").expect("the home writer is present");

    let manifest_digest = push_image(writer);
    assert_image_intact(writer, &manifest_digest);

    // The home datacenter fails and comes back on its own store. Two survivors keep a quorum, so the group
    // stays available through the outage; the returning home reopens the same durable store.
    let home = cluster
        .nodes_mut()
        .iter_mut()
        .find(|node| node.identity() == "writer-east")
        .unwrap();
    home.kill();
    await_down(home);
    home.restart().expect("the home datacenter restarts on its store");
    let writer = cluster.node("writer-east").expect("the home writer is back");
    await_image_served(writer);

    // Nothing was lost or corrupted: the same blobs, the same manifest, the same single tag.
    assert_image_intact(writer, &manifest_digest);

    // The retry after recovery resolves to the same content-addressed image, not a duplicate tag or a second
    // manifest.
    let retried_digest = push_image(writer);
    assert_eq!(
        retried_digest, manifest_digest,
        "the retry publishes the identical manifest digest",
    );
    assert_image_intact(writer, &manifest_digest);
}

#[test]
fn test_ha_replica_rejects_oci_mutations_before_and_after_a_home_failure() {
    // A read replica in another datacenter must refuse every OCI mutation, so no push is ever accepted off
    // the home authority. This holds through the home failing: killing the home writer never promotes a
    // replica to accept writes, which is what keeps the group from forking two homes.
    let mut cluster = home_oci_group();
    await_group_leader(&cluster);
    let replica = cluster.node("replica-west").expect("the replica is present");

    assert_replica_refuses_oci_mutations(replica);

    // The home datacenter dies. Two survivors elect a new leader, but the replica's read-only posture is
    // fixed at startup and a failover never flips it, so it still refuses every mutation.
    let home = cluster
        .nodes_mut()
        .iter_mut()
        .find(|node| node.identity() == "writer-east")
        .unwrap();
    home.kill();
    await_down(home);
    await_leader_change(&cluster, "east");

    let replica = cluster.node("replica-west").expect("the replica survives");
    assert!(
        replica.is_ready(),
        "the replica keeps serving reads after the home fails"
    );
    assert_replica_refuses_oci_mutations(replica);
}

/// Assert the replica refuses every OCI mutation verb with the same flat read-only rejection: a blob upload
/// `POST`, a manifest `PUT`, a manifest `DELETE`, and a blob `DELETE`. The availability middleware rejects by
/// HTTP method before routing reaches the OCI driver, so the body carries no credential and no digest need
/// exist.
fn assert_replica_refuses_oci_mutations(replica: &Node) {
    let route = crate::harness::OCI_ROUTE;
    let cases = [
        (reqwest::Method::POST, format!("/v2/{route}/{REPO}/blobs/uploads/")),
        (reqwest::Method::PUT, format!("/v2/{route}/{REPO}/manifests/1.0")),
        (reqwest::Method::DELETE, format!("/v2/{route}/{REPO}/manifests/1.0")),
        (
            reqwest::Method::DELETE,
            format!("/v2/{route}/{REPO}/blobs/{}", oci_digest(LAYER)),
        ),
    ];
    for (method, path) in cases {
        let (code, body) = replica
            .oci_mutate(method.clone(), &path)
            .expect("mutation reaches the replica");
        assert_eq!(
            (code, body.as_str()),
            (503, READ_ONLY_BODY),
            "the replica refuses a {method} {path}",
        );
    }
}
