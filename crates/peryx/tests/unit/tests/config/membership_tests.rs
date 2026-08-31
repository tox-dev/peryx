use rstest::rstest;

use crate::config::{self, AvailabilityConfig, AvailabilityMode, Config, ConfigError, DcMember, DcMembership, DcRole};

/// Keeps legacy replication roles covered during roster migration.
fn dc_config(members: &str) -> Result<Config, ConfigError> {
    let text = format!(
        "[availability]\nmode = \"dc\"\ngroup = \"east\"\n\
         [availability.replication]\nrole = \"primary\"\nsource = \"a\"\ntoken = \"t\"\n{members}"
    );
    config::from_toml("x.toml".into(), &text).and_then(|partial| Config::default().apply(partial))
}

fn ha_config(members: &str) -> Result<Config, ConfigError> {
    let text = format!(
        "[availability]\nmode = \"ha\"\ngroup = \"east\"\n\
         [availability.replication]\nrole = \"primary\"\nsource = \"a\"\ntoken = \"t\"\n\
         [availability.listener]\n{members}"
    );
    config::from_toml("x.toml".into(), &text).and_then(|partial| Config::default().apply(partial))
}

fn member(node: &str, dc: &str, address: &str, role: &str) -> String {
    format!("[[availability.member]]\nnode = \"{node}\"\ndc = \"{dc}\"\naddress = \"{address}\"\nrole = \"{role}\"\n")
}

fn writer_and_replica() -> String {
    member("node-a", "dc-1", "https://a:1", "writer") + &member("node-b", "dc-2", "https://b:1", "replica")
}

#[test]
fn test_valid_group_resolves_one_writer_and_a_replica() {
    let membership = dc_config(&writer_and_replica()).unwrap().dc_membership;
    assert_eq!(
        membership,
        Some(DcMembership {
            group: "east".to_owned(),
            members: vec![
                DcMember {
                    node: "node-a".to_owned(),
                    dc: "dc-1".to_owned(),
                    address: "https://a:1/".to_owned(),
                    role: DcRole::Writer,
                },
                DcMember {
                    node: "node-b".to_owned(),
                    dc: "dc-2".to_owned(),
                    address: "https://b:1/".to_owned(),
                    role: DcRole::Replica,
                },
            ],
        })
    );
}

#[rstest]
#[case::adds_the_root_path("https://a.internal:8443", "https://a.internal:8443/")]
#[case::lowercases_the_host("https://A.Internal:8443/", "https://a.internal:8443/")]
#[case::keeps_a_scheme_default_port("https://a.internal:443", "https://a.internal:443/")]
fn test_group_stores_the_canonical_member_address(#[case] address: &str, #[case] expected: &str) {
    let roster = member("w", "dc-1", address, "writer") + &member("r", "dc-2", "https://r:1", "replica");

    let membership = dc_config(&roster).unwrap().dc_membership.unwrap();

    assert_eq!(membership.members[0].address, expected);
}

/// A canonical address must survive the validation the resolved config repeats over it.
#[test]
fn test_a_canonical_member_address_revalidates() {
    let config = dc_config(&writer_and_replica()).unwrap();

    assert!(config.validate().is_ok());
}

#[test]
fn test_group_accepts_multiple_members_in_one_datacenter() {
    let roster = member("w", "dc-1", "https://w:1", "writer")
        + &member("r1", "dc-2", "https://r1:1", "replica")
        + &member("r2", "dc-2", "https://r2:1", "replica");
    let membership = dc_config(&roster).unwrap().dc_membership.unwrap();
    assert_eq!(membership.members.len(), 3);
    assert_eq!(
        membership.members.iter().filter(|m| m.role == DcRole::Replica).count(),
        2
    );
}

#[test]
fn test_ha_mode_accepts_a_roster_with_distinct_datacenters() {
    let membership = ha_config(&writer_and_replica()).unwrap().dc_membership.unwrap();
    assert_eq!(
        membership.members.iter().filter(|m| m.role == DcRole::Writer).count(),
        1
    );
}

#[test]
fn test_ha_mode_rejects_multiple_members_in_one_datacenter() {
    let roster = member("writer", "dc-east", "https://writer:1", "writer")
        + &member("replica", "dc-east", "https://replica:1", "replica");
    assert_eq!(
        ha_config(&roster).unwrap_err().to_string(),
        "datacenter membership: duplicate datacenter \"dc-east\" for node identities \"writer\" and \"replica\" in \
         `ha` mode"
    );
}

#[rstest]
#[case::writer(
    AvailabilityMode::Dc,
    Some("unknown"),
    None,
    "writer identity: `writer_identity` must name a configured `[[availability.member]]`"
)]
#[case::ha_node(
    AvailabilityMode::Ha,
    Some("node-a"),
    Some("unknown"),
    "availability: `node_identity` must name a configured `[[availability.member]]`"
)]
fn test_config_rejects_an_identity_outside_the_roster(
    #[case] mode: AvailabilityMode,
    #[case] writer_identity: Option<&str>,
    #[case] node_identity: Option<&str>,
    #[case] expected: &str,
) {
    let mut config = dc_config(&writer_and_replica()).unwrap();
    if mode == AvailabilityMode::Ha {
        config.availability = AvailabilityConfig::Ha(config.availability.replication().unwrap().clone());
    }
    config.writer_identity = writer_identity.map(str::to_owned);
    config.node_identity = node_identity.map(str::to_owned);
    assert_eq!(config.validate().unwrap_err().to_string(), expected);
}

#[rstest]
#[case::none_omitted(Config::default().dc_membership)]
#[case::none_from_role_only(
    config::from_toml(
        "x.toml".into(),
        "[availability]\nmode = \"dc\"\n[availability.replication]\nrole = \"primary\"\nsource = \"a\"\ntoken = \"t\"\n",
    )
    .and_then(|partial| Config::default().apply(partial))
    .unwrap()
    .dc_membership
)]
fn test_absent_roster_leaves_membership_unset(#[case] membership: Option<DcMembership>) {
    assert_eq!(membership, None);
}

#[test]
fn test_a_roster_requires_dc_or_ha_mode() {
    let text = format!("[availability]\ngroup = \"east\"\n{}", writer_and_replica());
    let error = config::from_toml("x.toml".into(), &text)
        .and_then(|partial| Config::default().apply(partial))
        .unwrap_err();
    assert!(error.to_string().contains("requires `dc` or `ha` mode"), "{error}");
}

#[rstest]
#[case::missing_group(
    format!("[availability]\nmode = \"dc\"\n[availability.replication]\nrole = \"primary\"\nsource = \"a\"\ntoken = \"t\"\n{}", writer_and_replica()),
    "needs a `group` identity"
)]
#[case::group_without_members(
    "[availability]\nmode = \"dc\"\ngroup = \"east\"\n[availability.replication]\nrole = \"primary\"\nsource = \"a\"\ntoken = \"t\"\n".to_owned(),
    "`group` needs at least one"
)]
#[case::blank_group(
    format!("[availability]\nmode = \"dc\"\ngroup = \" \"\n[availability.replication]\nrole = \"primary\"\nsource = \"a\"\ntoken = \"t\"\n{}", writer_and_replica()),
    "group must not be empty"
)]
fn test_group_and_roster_must_appear_together(#[case] text: String, #[case] expected: &str) {
    let error = config::from_toml("x.toml".into(), &text)
        .and_then(|partial| Config::default().apply(partial))
        .unwrap_err();
    assert!(error.to_string().contains(expected), "{error}");
}

#[rstest]
#[case::duplicate_node(
    member("dup", "dc-1", "https://a:1", "writer") + &member("dup", "dc-2", "https://b:1", "replica"),
    "duplicate node identity \"dup\""
)]
#[case::duplicate_address(
    member("w", "dc-1", "https://same:1", "writer") + &member("r", "dc-2", "https://same:1", "replica"),
    "duplicate advertised address \"https://same:1/\""
)]
#[case::node_matches_group(
    member("east", "dc-1", "https://a:1", "writer") + &member("r", "dc-2", "https://b:1", "replica"),
    "collides with the group identity"
)]
#[case::no_writer(
    member("r1", "dc-1", "https://a:1", "replica") + &member("r2", "dc-2", "https://b:1", "replica"),
    "needs exactly one writer"
)]
#[case::two_writers(
    member("w1", "dc-1", "https://a:1", "writer") + &member("w2", "dc-2", "https://b:1", "writer"),
    "allows only one writer"
)]
#[case::no_replica(
    member("w", "dc-1", "https://a:1", "writer"),
    "needs at least one configured replica"
)]
#[case::blank_node(member(" ", "dc-1", "https://a:1", "writer"), "member `node` must not be empty")]
#[case::blank_dc(
    member("w", " ", "https://w:1", "writer") + &member("r", "dc-2", "https://r:1", "replica"),
    "member `dc` must not be empty"
)]
#[case::blank_address(
    member("w", "dc-1", " ", "writer") + &member("r", "dc-2", "https://r:1", "replica"),
    "member address must not be empty"
)]
#[case::bare_host_address(
    member("w", "dc-1", "10.0.0.1:8443", "writer") + &member("r", "dc-2", "https://r:1", "replica"),
    "is not a valid URL"
)]
#[case::non_http_scheme_address(
    member("w", "dc-1", "ftp://a:1", "writer") + &member("r", "dc-2", "https://r:1", "replica"),
    "must use the http or https scheme"
)]
#[case::non_url_address(
    member("w", "dc-1", "not a url", "writer") + &member("r", "dc-2", "https://r:1", "replica"),
    "is not a valid URL"
)]
#[case::missing_port_address(
    member("w", "dc-1", "https://a.internal", "writer") + &member("r", "dc-2", "https://r:1", "replica"),
    "needs an explicit `host:port`"
)]
#[case::base_path_address(
    member("w", "dc-1", "https://a.internal:8443/raft", "writer") + &member("r", "dc-2", "https://r:1", "replica"),
    "no path, query, fragment, or credentials"
)]
#[case::query_address(
    member("w", "dc-1", "https://a.internal:8443/?dc=east", "writer") + &member("r", "dc-2", "https://r:1", "replica"),
    "no path, query, fragment, or credentials"
)]
#[case::credentials_address(
    member("w", "dc-1", "https://peer:secret@a.internal:8443", "writer") + &member("r", "dc-2", "https://r:1", "replica"),
    "no path, query, fragment, or credentials"
)]
#[case::equivalent_address(
    member("w", "dc-1", "https://Same:443", "writer") + &member("r", "dc-2", "https://same:443/", "replica"),
    "duplicate advertised address"
)]
fn test_group_rejects_invalid_topologies(#[case] roster: String, #[case] expected: &str) {
    let error = dc_config(&roster).unwrap_err();
    assert!(error.to_string().contains(expected), "{error}");
}

#[rstest]
#[case::blank(" ", "member address must not be empty")]
#[case::bare_host("10.0.0.1:8443", "is not a valid URL")]
#[case::non_http_scheme("ftp://a:1", "must use the http or https scheme")]
#[case::non_url("not a url", "is not a valid URL")]
#[case::missing_port("https://a.internal", "needs an explicit `host:port`")]
#[case::base_path("https://a.internal:8443/raft", "no path, query, fragment, or credentials")]
fn test_resolved_config_rejects_an_invalid_member_address(#[case] address: &str, #[case] expected: &str) {
    let mut config = dc_config(&writer_and_replica()).unwrap();
    config.dc_membership.as_mut().unwrap().members[0].address = address.to_owned();

    let error = config.validate().unwrap_err();
    assert!(error.to_string().contains(expected), "{error}");
}

#[rstest]
#[case::unknown_role(member("w", "dc-1", "https://a:1", "observer"), "unknown variant `observer`")]
#[case::unknown_field(
    "[[availability.member]]\nnode = \"w\"\ndc = \"dc-1\"\naddress = \"https://a:1\"\nrole = \"writer\"\nregion = \"x\"\n".to_owned(),
    "unknown field `region`"
)]
fn test_member_table_rejects_bad_fields(#[case] roster: String, #[case] expected: &str) {
    let error = config::from_toml(
        "x.toml".into(),
        &format!("[availability]\nmode = \"dc\"\ngroup = \"east\"\n{roster}"),
    )
    .unwrap_err();
    assert!(error.to_string().contains(expected), "{error}");
}
