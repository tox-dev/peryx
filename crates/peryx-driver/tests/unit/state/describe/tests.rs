use super::{MemberDescription, describe_index, describe_indexes, describe_upstream_route};
use peryx_core::Ecosystem;
use peryx_identity::{Action, Glob, Grant, IndexAcl, NamedToken};
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use peryx_upstream::{NamedUpstream, UpstreamClient, UpstreamRouter};
use rstest::rstest;

fn named_token(name: &str, actions: &[Action], expires_at: Option<i64>) -> NamedToken {
    NamedToken {
        name: name.to_owned(),
        secret: "secret".to_owned(),
        grants: vec![Grant {
            resources: vec![Glob::new("*")],
            actions: actions.iter().copied().collect(),
        }],
        expires_at,
    }
}

fn token_acl(actions: &[Action], expires_at: Option<i64>) -> IndexAcl {
    IndexAcl {
        anonymous_read: true,
        tokens: vec![named_token("token", actions, expires_at)],
    }
}

fn index(name: &str, kind: IndexKind, acl: IndexAcl) -> Index {
    Index {
        name: name.to_owned(),
        route: name.to_owned(),
        ecosystem: Ecosystem::new("example"),
        kind,
        policy: Policy::default(),
        acl,
    }
}

fn cached() -> IndexKind {
    IndexKind::Cached {
        client: UpstreamClient::new("http://example.invalid/artifacts/").unwrap(),
        offline: false,
    }
}

fn member(name: &str, role: &'static str) -> MemberDescription {
    MemberDescription {
        name: name.to_owned(),
        role,
    }
}

fn route() -> UpstreamRouter {
    UpstreamRouter::new(
        ["first", "second"]
            .into_iter()
            .map(|name| {
                NamedUpstream::new(
                    name,
                    UpstreamClient::new(&format!("https://{name}.example/artifacts/")).unwrap(),
                )
            })
            .collect(),
    )
    .unwrap()
}

#[test]
fn test_cached_index_names_its_role_and_lists_no_members() {
    let indexes = vec![index("alpha", cached(), IndexAcl::default())];
    let described = describe_index(&indexes, 0);
    assert_eq!(described.kind, "cached");
    assert!(described.layers.is_empty());
    assert!(described.precedence.is_empty());
}

#[test]
fn test_describe_indexes_preserves_input_order() {
    let indexes = vec![
        index("first", IndexKind::Hosted { volatile: false }, IndexAcl::default()),
        index("second", IndexKind::Hosted { volatile: false }, IndexAcl::default()),
    ];

    assert_eq!(
        describe_indexes(&indexes)
            .into_iter()
            .map(|index| index.name)
            .collect::<Vec<_>>(),
        vec!["first", "second"]
    );
}

#[rstest]
#[case::hosted_upload_only(false, &[Action::Write], true, true, false)]
#[case::hosted_delete_only(false, &[Action::Delete], true, false, true)]
#[case::hosted_both(false, &[Action::Write, Action::Delete], true, true, true)]
#[case::hosted_stable_delete(false, &[Action::Delete], false, false, false)]
#[case::virtual_upload_only(true, &[Action::Write], true, true, false)]
#[case::virtual_delete_only(true, &[Action::Delete], true, false, true)]
#[case::virtual_both(true, &[Action::Write, Action::Delete], true, true, true)]
fn test_describe_index_uses_each_active_action(
    #[case] virtual_index: bool,
    #[case] actions: &[Action],
    #[case] volatile: bool,
    #[case] expected_uploads: bool,
    #[case] expected_deletes: bool,
) {
    let mut indexes = vec![index("store", IndexKind::Hosted { volatile }, token_acl(actions, None))];
    let position = if virtual_index {
        indexes.push(index(
            "virtual",
            IndexKind::Virtual {
                layers: vec![0],
                write_target: Some(0),
            },
            IndexAcl::default(),
        ));
        1
    } else {
        0
    };

    let described = describe_index(&indexes, position);
    assert_eq!(
        (described.uploads, described.volatile_deletes),
        (expected_uploads, expected_deletes)
    );
}

#[test]
fn test_expired_grants_disable_capabilities_but_remain_configured() {
    let indexes = vec![index(
        "store",
        IndexKind::Hosted { volatile: true },
        token_acl(&[Action::Write, Action::Delete], Some(0)),
    )];

    let described = describe_index(&indexes, 0);
    assert_eq!((described.uploads, described.volatile_deletes), (false, false));
    assert!(described.hosted.unwrap().upload_token.configured);
}

#[test]
fn test_active_grant_for_another_token_keeps_its_capability() {
    let mut acl = token_acl(&[Action::Write], Some(0));
    acl.tokens
        .push(named_token("active", &[Action::Delete], Some(i64::MAX)));
    let indexes = vec![index("store", IndexKind::Hosted { volatile: true }, acl)];

    let described = describe_index(&indexes, 0);
    assert_eq!((described.uploads, described.volatile_deletes), (false, true));
}

#[test]
fn test_removing_a_token_removes_its_capabilities() {
    let mut indexes = vec![index(
        "store",
        IndexKind::Hosted { volatile: true },
        token_acl(&[Action::Write, Action::Delete], None),
    )];
    indexes[0].acl.tokens.clear();

    let described = describe_index(&indexes, 0);
    assert_eq!((described.uploads, described.volatile_deletes), (false, false));
    assert!(!described.hosted.unwrap().upload_token.configured);
}

#[test]
fn test_virtual_precedence_forces_cached_members_last_and_tags_roles() {
    let indexes = vec![
        index("alpha", cached(), IndexAcl::default()),
        index("local", IndexKind::Hosted { volatile: false }, IndexAcl::default()),
        index(
            "mix",
            IndexKind::Virtual {
                layers: vec![0, 1],
                write_target: None,
            },
            IndexAcl::default(),
        ),
    ];
    let described = describe_index(&indexes, 2);
    assert_eq!(described.layers, vec!["alpha".to_owned(), "local".to_owned()]);
    assert_eq!(
        described.precedence,
        vec![member("local", "hosted"), member("alpha", "cached")]
    );
}

#[test]
fn test_virtual_upload_target_drives_uploads_and_volatile_deletes() {
    let indexes = vec![
        index(
            "store",
            IndexKind::Hosted { volatile: true },
            token_acl(&[Action::Write, Action::Delete], None),
        ),
        index(
            "v",
            IndexKind::Virtual {
                layers: vec![0],
                write_target: Some(0),
            },
            IndexAcl::default(),
        ),
    ];
    let described = describe_index(&indexes, 1);
    assert!(described.uploads);
    assert_eq!(described.upload_to.as_deref(), Some("store"));
    assert_eq!(described.precedence, vec![member("store", "hosted")]);
}

#[test]
fn test_upstream_route_status_tracks_each_aggregate_state() {
    let route = route();
    assert_eq!(describe_upstream_route(&route).0, "configured");

    route.sources().next().unwrap().mark_healthy();
    assert_eq!(describe_upstream_route(&route).0, "healthy");

    route.sources().nth(1).unwrap().mark_unhealthy();
    assert_eq!(describe_upstream_route(&route).0, "degraded");

    route.sources().next().unwrap().mark_unhealthy();
    assert_eq!(describe_upstream_route(&route).0, "unhealthy");
}
