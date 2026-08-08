use std::sync::Arc;

use super::{WorkerShared, worker_reason};

#[test]
fn test_worker_reason_names_only_a_failed_domain() {
    assert_eq!(worker_reason(None), None);
    let healthy = Arc::new(WorkerShared::for_replica());
    assert_eq!(worker_reason(Some(&healthy)), None);
    let failed = Arc::new(WorkerShared::for_replica());
    failed.record_panic();
    assert_eq!(worker_reason(Some(&failed)), Some("worker_unhealthy"));
}

fn member(node: &str, address: &str, role: crate::config::DcRole) -> crate::config::DcMember {
    crate::config::DcMember {
        node: node.to_owned(),
        dc: format!("dc-{node}"),
        address: address.to_owned(),
        role,
    }
}

#[test]
fn test_metadata_peers_enumerates_the_roster_minus_this_node() {
    use crate::config::{DcMembership, DcRole};

    let membership = DcMembership {
        group: "group".to_owned(),
        members: vec![
            member("writer", "https://writer.example/", DcRole::Writer),
            member("replica-b", "https://replica-b.example/", DcRole::Replica),
            member("replica-c", "https://replica-c.example/", DcRole::Replica),
            // A duplicate address is joined once; a second member on it is skipped.
            member("replica-c-alias", "https://replica-c.example/", DcRole::Replica),
        ],
    };

    let set = super::metadata_peers(
        Some(&membership),
        Some("replica-b"),
        "https://writer.example/",
        "tok",
        7,
        std::num::NonZeroUsize::new(256).unwrap(),
    )
    .unwrap();

    // This node is excluded, the writer's address matches the upstream so it is joined once, and the
    // duplicate address is not rejoined. Every peer resumes at the replica's durable serial.
    let mut sources = set.sources();
    sources.sort();
    assert_eq!(sources, vec!["replica-c".to_owned(), "writer".to_owned()]);
    assert_eq!(set.frontier("writer"), Some(7));
    assert_eq!(set.frontier("replica-c"), Some(7));
}

#[test]
fn test_metadata_peers_falls_back_to_the_upstream_without_a_roster() {
    let set = super::metadata_peers(
        None,
        None,
        "https://writer.example/",
        "tok",
        0,
        std::num::NonZeroUsize::new(256).unwrap(),
    )
    .unwrap();
    assert_eq!(set.sources(), vec![super::UPSTREAM_SOURCE.to_owned()]);
}

#[test]
fn test_metadata_peers_rejects_an_unusable_member_address() {
    use crate::config::{DcMembership, DcRole};

    let membership = DcMembership {
        group: "group".to_owned(),
        members: vec![member("bad", "not a url", DcRole::Replica)],
    };

    let built = super::metadata_peers(
        Some(&membership),
        Some("self"),
        "https://writer.example/",
        "tok",
        0,
        std::num::NonZeroUsize::new(256).unwrap(),
    );

    assert!(built.is_err());
}
