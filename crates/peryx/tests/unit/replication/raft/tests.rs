use super::*;

#[test]
fn non_ha_modes_have_no_consensus_plan() {
    assert!(consensus_plan(&Config::default()).unwrap().is_none());
}

#[test]
fn roster_must_contain_the_local_node() {
    let config = Config {
        node_identity: Some("local".to_owned()),
        availability: AvailabilityConfig::Ha(ReplicationConfig::Primary {
            source: "peer".to_owned(),
            token: crate::config::SecretSource::Literal("token".to_owned()),
        }),
        dc_membership: Some(crate::config::DcMembership {
            group: "group".to_owned(),
            members: vec![crate::config::DcMember {
                node: "peer".to_owned(),
                dc: "east".to_owned(),
                address: "http://peer/".to_owned(),
                role: DcRole::Writer,
            }],
        }),
        ..Config::default()
    };

    let error = consensus_plan(&config).err().unwrap();
    assert!(error.to_string().contains("not a member"));
}
