use std::sync::Arc;

use peryx_core::TopologyMode;
use peryx_driver::state::AppState;

use crate::config::{AvailabilityConfig, Config, DcMember, DcMembership, DcRole, ReplicationConfig, SecretSource};
use crate::server::build_state;

fn build(mut config: Config) -> anyhow::Result<(tempfile::TempDir, Arc<AppState>)> {
    let dir = tempfile::tempdir().unwrap();
    config.data_dir = dir.path().to_path_buf();
    build_state(&config).map(|state| (dir, state))
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
fn test_installs_a_reader_reaching_peers_by_http_and_https() {
    let membership = roster(vec![
        member("node-a", "dc-1", "http://10.0.0.1:8080", DcRole::Writer),
        member("node-b", "dc-2", "http://10.0.0.2:8080", DcRole::Replica),
        member("node-c", "dc-3", "https://peer-c.example:8443", DcRole::Replica),
    ]);

    let (_dir, state) = build(config(dc_primary(), Some(membership), Some("node-a"))).unwrap();
    assert_eq!(state.serving.meta.count_artifact_placements().unwrap(), 0);
    let topology = state.serving.availability_topology();
    assert_eq!(topology.mode, TopologyMode::Dc);
    assert_eq!(topology.group.as_deref(), Some("g"));
    assert_eq!(
        topology
            .members
            .iter()
            .map(|member| member.address.as_str())
            .collect::<Vec<_>>(),
        [
            "http://10.0.0.1:8080",
            "http://10.0.0.2:8080",
            "https://peer-c.example:8443"
        ]
    );
}

#[test]
fn test_install_fails_on_an_unusable_peer_address() {
    let membership = roster(vec![
        member("node-a", "dc-1", "http://10.0.0.1:8080", DcRole::Writer),
        member("node-b", "dc-2", "http://a b", DcRole::Replica),
    ]);

    let err = build(config(dc_primary(), Some(membership), Some("node-a")))
        .err()
        .expect("invalid peer address must fail startup");

    assert!(
        format!("{err:#}").contains("member address \"http://a b\" is not a valid URL"),
        "{err:#}"
    );
}

#[test]
fn test_install_fails_on_an_invalid_local_datacenter() {
    let membership = roster(vec![
        member("node-a", "", "http://10.0.0.1:8080", DcRole::Writer),
        member("node-b", "dc-2", "http://10.0.0.2:8080", DcRole::Replica),
    ]);

    let err = build(config(dc_primary(), Some(membership), Some("node-a")))
        .err()
        .expect("invalid local datacenter must fail startup");

    assert!(err.to_string().contains("local datacenter identity"), "{err}");
}

#[test]
fn test_no_reader_without_availability_replication() {
    let (_dir, state) = build(config(AvailabilityConfig::None, None, None)).unwrap();
    assert_eq!(state.serving.availability_topology().mode, TopologyMode::None);
}

#[test]
fn test_no_reader_without_a_membership_roster() {
    let (_dir, state) = build(config(dc_primary(), None, Some("node-a"))).unwrap();
    let topology = state.serving.availability_topology();
    assert_eq!(topology.mode, TopologyMode::Dc);
    assert!(topology.members.is_empty());
}

#[test]
fn test_no_reader_when_the_node_identity_is_unknown() {
    let membership = roster(vec![member("node-a", "dc-1", "https://a:1", DcRole::Writer)]);

    let (_dir, state) = build(config(dc_primary(), Some(membership), None)).unwrap();
    assert_eq!(state.serving.availability_topology().local_node, None);
}

#[test]
fn test_no_reader_when_the_roster_has_no_remote_peer() {
    let membership = roster(vec![member("node-a", "dc-1", "https://a:1", DcRole::Writer)]);

    let (_dir, state) = build(config(dc_primary(), Some(membership), Some("node-a"))).unwrap();
    assert_eq!(state.serving.availability_topology().members.len(), 1);
}

#[test]
fn test_the_reader_resolves_the_local_datacenter_from_the_node_identity() {
    let membership = roster(vec![
        member("node-a", "dc-1", "http://10.0.0.1:8080", DcRole::Writer),
        member("node-b", "dc-2", "http://10.0.0.2:8080", DcRole::Replica),
        member("node-c", "dc-3", "http://10.0.0.3:8080", DcRole::Replica),
    ]);
    let config = Config {
        node_identity: Some("node-b".to_owned()),
        ..config(
            AvailabilityConfig::Ha(ReplicationConfig::Primary {
                source: "a".to_owned(),
                token: SecretSource::Literal("t".to_owned()),
            }),
            Some(membership),
            Some("node-a"),
        )
    };

    let (_dir, state) = build(config).unwrap();
    assert_eq!(state.serving.availability_topology().mode, TopologyMode::Ha);
}
