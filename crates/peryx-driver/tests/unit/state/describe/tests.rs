use std::collections::BTreeSet;

use super::{MemberDescription, describe_index, describe_indexes, describe_upstream_route};
use peryx_core::Ecosystem;
use peryx_identity::{Action, Glob, Grant, IndexAcl, NamedToken};
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use peryx_upstream::{NamedUpstream, UpstreamClient, UpstreamRouter};
use rstest::rstest;

fn writer_acl(secret: impl Into<String>) -> IndexAcl {
    IndexAcl {
        anonymous_read: true,
        tokens: vec![NamedToken {
            name: "uploader".to_owned(),
            secret: secret.into(),
            grants: vec![Grant {
                resources: vec![Glob::new("*")],
                actions: BTreeSet::from([Action::Write, Action::Delete]),
            }],
            expires_at: None,
        }],
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
#[case::hosted_read_only_stable(false, false, false, false)]
#[case::hosted_read_only_volatile(false, false, true, false)]
#[case::hosted_writable_stable(false, true, false, false)]
#[case::hosted_writable_volatile(false, true, true, true)]
#[case::virtual_read_only_stable(true, false, false, false)]
#[case::virtual_read_only_volatile(true, false, true, false)]
#[case::virtual_writable_stable(true, true, false, false)]
#[case::virtual_writable_volatile(true, true, true, true)]
fn test_volatile_deletes_require_a_writable_volatile_target(
    #[case] virtual_index: bool,
    #[case] writable: bool,
    #[case] volatile: bool,
    #[case] expected: bool,
) {
    let acl = if writable { writer_acl("s") } else { IndexAcl::default() };
    let mut indexes = vec![index("store", IndexKind::Hosted { volatile }, acl)];
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

    assert_eq!(describe_index(&indexes, position).volatile_deletes, expected);
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
        index("store", IndexKind::Hosted { volatile: true }, writer_acl("s")),
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
