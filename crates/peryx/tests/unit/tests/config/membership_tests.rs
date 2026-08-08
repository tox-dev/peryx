use rstest::rstest;

use crate::config::{self, Config, ConfigError, DcMember, DcMembership, DcRole};

/// A `dc` node still carries its legacy `[availability.replication]` role during the migration window,
/// so a member roster is validated alongside it. `members` is the raw `[[availability.member]]` block.
fn dc_config(members: &str) -> Result<Config, ConfigError> {
    let text = format!(
        "[availability]\nmode = \"dc\"\ngroup = \"east\"\n\
         [availability.replication]\nrole = \"primary\"\nsource = \"a\"\ntoken = \"t\"\n{members}"
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
                    address: "https://a:1".to_owned(),
                    role: DcRole::Writer,
                },
                DcMember {
                    node: "node-b".to_owned(),
                    dc: "dc-2".to_owned(),
                    address: "https://b:1".to_owned(),
                    role: DcRole::Replica,
                },
            ],
        })
    );
}

#[test]
fn test_group_accepts_multiple_configured_replicas() {
    let roster = member("w", "dc-1", "https://w:1", "writer")
        + &member("r1", "dc-2", "https://r1:1", "replica")
        + &member("r2", "dc-3", "https://r2:1", "replica");
    let membership = dc_config(&roster).unwrap().dc_membership.unwrap();
    assert_eq!(membership.members.len(), 3);
    assert_eq!(
        membership.members.iter().filter(|m| m.role == DcRole::Replica).count(),
        2
    );
}

#[test]
fn test_ha_mode_also_accepts_a_roster() {
    let text = format!(
        "[availability]\nmode = \"ha\"\ngroup = \"east\"\n\
         [availability.replication]\nrole = \"primary\"\nsource = \"a\"\ntoken = \"t\"\n{}",
        writer_and_replica()
    );
    let membership = config::from_toml("x.toml".into(), &text)
        .and_then(|partial| Config::default().apply(partial))
        .unwrap()
        .dc_membership
        .unwrap();
    assert_eq!(
        membership.members.iter().filter(|m| m.role == DcRole::Writer).count(),
        1
    );
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
#[case::duplicate_datacenter(
    member("w", "same", "https://a:1", "writer") + &member("r", "same", "https://b:1", "replica"),
    "duplicate datacenter identity \"same\""
)]
#[case::duplicate_address(
    member("w", "dc-1", "https://same:1", "writer") + &member("r", "dc-2", "https://same:1", "replica"),
    "duplicate advertised address \"https://same:1\""
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
    "member `address` must not be empty"
)]
#[case::bare_host_address(
    member("w", "dc-1", "10.0.0.1:8443", "writer") + &member("r", "dc-2", "https://r:1", "replica"),
    "must be an http or https URL"
)]
#[case::non_http_scheme_address(
    member("w", "dc-1", "ftp://a:1", "writer") + &member("r", "dc-2", "https://r:1", "replica"),
    "must be an http or https URL"
)]
#[case::non_url_address(
    member("w", "dc-1", "not a url", "writer") + &member("r", "dc-2", "https://r:1", "replica"),
    "must be an http or https URL"
)]
fn test_group_rejects_invalid_topologies(#[case] roster: String, #[case] expected: &str) {
    let error = dc_config(&roster).unwrap_err();
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
