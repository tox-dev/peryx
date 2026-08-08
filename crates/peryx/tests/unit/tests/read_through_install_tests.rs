use std::sync::Arc;

use peryx_driver::state::AppState;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

use crate::config::{AvailabilityConfig, Config, DcMember, DcMembership, DcRole, ReplicationConfig, SecretSource};
use crate::replication::install_read_through;

fn state() -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    (dir, Arc::new(AppState::new(meta, blobs, 60, Vec::new())))
}

fn member(node: &str, dc: &str, address: &str, role: DcRole) -> DcMember {
    DcMember {
        node: node.to_owned(),
        dc: dc.to_owned(),
        address: address.to_owned(),
        role,
    }
}

fn dc_primary() -> AvailabilityConfig {
    AvailabilityConfig::Dc(ReplicationConfig::Primary {
        source: "a".to_owned(),
        token: SecretSource::Literal("t".to_owned()),
    })
}

fn config(availability: AvailabilityConfig, membership: Option<DcMembership>, writer_identity: Option<&str>) -> Config {
    Config {
        availability,
        dc_membership: membership,
        writer_identity: writer_identity.map(str::to_owned),
        ..Config::default()
    }
}

fn roster(members: Vec<DcMember>) -> DcMembership {
    DcMembership {
        group: "g".to_owned(),
        members,
    }
}

#[test]
fn test_installs_a_reader_reaching_peers_by_bare_address_and_full_url() {
    let (_dir, state) = state();
    let membership = roster(vec![
        member("node-a", "dc-1", "10.0.0.1:8080", DcRole::Writer),
        member("node-b", "dc-2", "10.0.0.2:8080", DcRole::Replica),
        member("node-c", "dc-3", "https://peer-c.example:8443", DcRole::Replica),
    ]);

    install_read_through(&config(dc_primary(), Some(membership), Some("node-a")), &state).unwrap();

    assert!(state.read_through().is_some());
}

#[test]
fn test_install_fails_on_an_unusable_peer_address() {
    let (_dir, state) = state();
    let membership = roster(vec![
        member("node-a", "dc-1", "10.0.0.1:8080", DcRole::Writer),
        member("node-b", "dc-2", "http://a b", DcRole::Replica),
    ]);

    let err = install_read_through(&config(dc_primary(), Some(membership), Some("node-a")), &state).unwrap_err();

    assert!(
        err.to_string()
            .contains("read-through blob transport for datacenter dc-2"),
        "{err}"
    );
    assert!(state.read_through().is_none());
}

#[test]
fn test_install_fails_on_an_invalid_local_datacenter() {
    let (_dir, state) = state();
    let membership = roster(vec![
        member("node-a", "", "10.0.0.1:8080", DcRole::Writer),
        member("node-b", "dc-2", "10.0.0.2:8080", DcRole::Replica),
    ]);

    let err = install_read_through(&config(dc_primary(), Some(membership), Some("node-a")), &state).unwrap_err();

    assert!(err.to_string().contains("local datacenter identity"), "{err}");
    assert!(state.read_through().is_none());
}

#[test]
fn test_no_reader_without_availability_replication() {
    let (_dir, state) = state();

    install_read_through(&config(AvailabilityConfig::None, None, None), &state).unwrap();

    assert!(state.read_through().is_none());
}

#[test]
fn test_no_reader_without_a_membership_roster() {
    let (_dir, state) = state();

    install_read_through(&config(dc_primary(), None, Some("node-a")), &state).unwrap();

    assert!(state.read_through().is_none());
}

#[test]
fn test_no_reader_when_the_node_identity_is_unknown() {
    let (_dir, state) = state();
    let membership = roster(vec![member("node-a", "dc-1", "https://a:1", DcRole::Writer)]);

    install_read_through(&config(dc_primary(), Some(membership), None), &state).unwrap();

    assert!(state.read_through().is_none());
}

#[test]
fn test_no_reader_when_the_roster_has_no_remote_peer() {
    let (_dir, state) = state();
    let membership = roster(vec![member("node-a", "dc-1", "https://a:1", DcRole::Writer)]);

    install_read_through(&config(dc_primary(), Some(membership), Some("node-a")), &state).unwrap();

    assert!(state.read_through().is_none());
}

#[test]
fn test_the_reader_resolves_the_local_datacenter_from_the_node_identity() {
    // writer_identity is the one writer every node claims, so a replica must place itself by
    // node_identity. Here writer_identity names a node absent from the roster while node_identity names
    // this replica's own dc-2 member; the reader installs only because node_identity resolves the local
    // datacenter, leaving dc-3 as the remote peer to reach.
    let (_dir, state) = state();
    let membership = roster(vec![
        member("node-b", "dc-2", "10.0.0.2:8080", DcRole::Replica),
        member("node-c", "dc-3", "10.0.0.3:8080", DcRole::Replica),
    ]);
    let config = Config {
        node_identity: Some("node-b".to_owned()),
        ..config(dc_primary(), Some(membership), Some("node-a"))
    };

    install_read_through(&config, &state).unwrap();

    assert!(state.read_through().is_some());
}
