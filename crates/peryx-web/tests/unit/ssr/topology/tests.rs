use peryx_core::{LocalStatus, NodeLiveness, NodeRole, TopologyView};
use peryx_http::response_security::FieldClassification;

use super::{local_status_from_observations, topology_view_for_class};

#[test]
fn topology_projects_authority_and_health() {
    for (class, expected) in [
        (None, TopologyView::Public),
        (Some(FieldClassification::Public), TopologyView::Public),
        (Some(FieldClassification::Repository), TopologyView::Public),
        (Some(FieldClassification::Operator), TopologyView::Operator),
        (Some(FieldClassification::Administrator), TopologyView::Administrator),
    ] {
        assert_eq!(topology_view_for_class(class), expected);
    }
    for (serial, blobs_healthy, expected) in [
        (
            Some(7),
            true,
            LocalStatus {
                role: NodeRole::Writer,
                liveness: NodeLiveness::Live,
                frontier: 7,
            },
        ),
        (
            Some(7),
            false,
            LocalStatus {
                role: NodeRole::Writer,
                liveness: NodeLiveness::Unready,
                frontier: 7,
            },
        ),
        (
            None,
            true,
            LocalStatus {
                role: NodeRole::Writer,
                liveness: NodeLiveness::Unready,
                frontier: 0,
            },
        ),
    ] {
        assert_eq!(
            local_status_from_observations(NodeRole::Writer, serial, blobs_healthy),
            expected
        );
    }
}
