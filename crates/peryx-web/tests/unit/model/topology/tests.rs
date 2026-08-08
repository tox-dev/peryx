use peryx_core::{NodeLiveness, NodeRole, TopologyMode};
use rstest::rstest;

use super::{RoleFilter, StreamStatus, liveness_health, mode_label, role_label, stream_status_label};

#[rstest]
#[case(Some(NodeLiveness::Live), "Live", "health-live")]
#[case(Some(NodeLiveness::Unready), "Unready", "health-unready")]
#[case(Some(NodeLiveness::Unknown), "Unknown", "health-unknown")]
#[case(None, "Restricted", "health-restricted")]
fn test_liveness_health_labels_every_state_with_text(
    #[case] liveness: Option<NodeLiveness>,
    #[case] text: &str,
    #[case] class: &str,
) {
    let label = liveness_health(liveness);
    assert_eq!(label.text, text);
    assert_eq!(label.class, class);
}

#[test]
fn test_withheld_liveness_never_reads_as_healthy() {
    assert_eq!(liveness_health(None).text, "Restricted");
    assert_ne!(
        liveness_health(None).class,
        liveness_health(Some(NodeLiveness::Live)).class
    );
}

#[rstest]
#[case(StreamStatus::Live, "Live", "health-live")]
#[case(StreamStatus::Connecting, "Reconnecting", "health-unready")]
#[case(StreamStatus::Stale, "Stale", "health-unready")]
#[case(StreamStatus::Offline, "Offline", "health-unknown")]
fn test_stream_status_labels_every_state_with_text(
    #[case] status: StreamStatus,
    #[case] text: &str,
    #[case] class: &str,
) {
    let label = stream_status_label(status);
    assert_eq!(label.text, text);
    assert_eq!(label.class, class);
}

#[test]
fn test_frozen_feed_never_reads_as_live() {
    assert_ne!(
        stream_status_label(StreamStatus::Offline).class,
        stream_status_label(StreamStatus::Live).class,
    );
    assert_ne!(
        stream_status_label(StreamStatus::Connecting).class,
        stream_status_label(StreamStatus::Live).class,
    );
    assert_ne!(
        stream_status_label(StreamStatus::Stale).class,
        stream_status_label(StreamStatus::Live).class,
    );
}

#[test]
fn test_stream_status_starts_out_of_live() {
    assert_ne!(StreamStatus::default(), StreamStatus::Live);
    assert_eq!(StreamStatus::default(), StreamStatus::Connecting);
}

#[rstest]
#[case(NodeRole::Writer, "Writer")]
#[case(NodeRole::Replica, "Replica")]
fn test_role_label(#[case] role: NodeRole, #[case] label: &str) {
    assert_eq!(role_label(role), label);
}

#[rstest]
#[case(TopologyMode::None, "Single node")]
#[case(TopologyMode::Dc, "Datacenter")]
#[case(TopologyMode::Ha, "High availability")]
fn test_mode_label(#[case] mode: TopologyMode, #[case] label: &str) {
    assert_eq!(mode_label(mode), label);
}

#[rstest]
#[case("writer", RoleFilter::Writer)]
#[case("replica", RoleFilter::Replica)]
#[case("all", RoleFilter::All)]
#[case("", RoleFilter::All)]
#[case("nonsense", RoleFilter::All)]
fn test_role_filter_parses_value(#[case] value: &str, #[case] filter: RoleFilter) {
    assert_eq!(RoleFilter::from_value(value), filter);
}

#[rstest]
#[case(RoleFilter::All, NodeRole::Writer, true)]
#[case(RoleFilter::All, NodeRole::Replica, true)]
#[case(RoleFilter::Writer, NodeRole::Writer, true)]
#[case(RoleFilter::Writer, NodeRole::Replica, false)]
#[case(RoleFilter::Replica, NodeRole::Replica, true)]
#[case(RoleFilter::Replica, NodeRole::Writer, false)]
fn test_role_filter_matches(#[case] filter: RoleFilter, #[case] role: NodeRole, #[case] matches: bool) {
    assert_eq!(filter.matches(role), matches);
}
