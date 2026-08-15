use peryx_core::{NodeLiveness, NodeRole, TopologyMode};

use super::{HealthLabel, RoleFilter, StreamStatus, liveness_health, mode_label, role_label, stream_status_label};

#[test]
fn test_liveness_health_labels_every_state() {
    for (liveness, expected) in [
        (
            Some(NodeLiveness::Live),
            HealthLabel {
                text: "Live",
                class: "health-live",
            },
        ),
        (
            Some(NodeLiveness::Unready),
            HealthLabel {
                text: "Unready",
                class: "health-unready",
            },
        ),
        (
            Some(NodeLiveness::Unknown),
            HealthLabel {
                text: "Unknown",
                class: "health-unknown",
            },
        ),
        (
            None,
            HealthLabel {
                text: "Restricted",
                class: "health-restricted",
            },
        ),
    ] {
        assert_eq!(liveness_health(liveness), expected);
    }
}

#[test]
fn test_stream_status_labels_every_state() {
    for (status, expected) in [
        (
            StreamStatus::Live,
            HealthLabel {
                text: "Live",
                class: "health-live",
            },
        ),
        (
            StreamStatus::Connecting,
            HealthLabel {
                text: "Reconnecting",
                class: "health-unready",
            },
        ),
        (
            StreamStatus::Stale,
            HealthLabel {
                text: "Stale",
                class: "health-unready",
            },
        ),
        (
            StreamStatus::Offline,
            HealthLabel {
                text: "Offline",
                class: "health-unknown",
            },
        ),
    ] {
        assert_eq!(stream_status_label(status), expected);
    }
}

#[test]
fn test_stream_status_starts_out_of_live() {
    assert_eq!(StreamStatus::default(), StreamStatus::Connecting);
}

#[test]
fn test_role_label() {
    for (role, expected) in [(NodeRole::Writer, "Writer"), (NodeRole::Replica, "Replica")] {
        assert_eq!(role_label(role), expected);
    }
}

#[test]
fn test_mode_label() {
    for (mode, expected) in [
        (TopologyMode::None, "Single node"),
        (TopologyMode::Dc, "Datacenter"),
        (TopologyMode::Ha, "High availability"),
    ] {
        assert_eq!(mode_label(mode), expected);
    }
}

#[test]
fn test_role_filter_parses_value() {
    for (value, expected) in [
        ("writer", RoleFilter::Writer),
        ("replica", RoleFilter::Replica),
        ("all", RoleFilter::All),
        ("", RoleFilter::All),
        ("nonsense", RoleFilter::All),
    ] {
        assert_eq!(RoleFilter::from_value(value), expected);
    }
}

#[test]
fn test_role_filter_matches() {
    for (filter, role, expected) in [
        (RoleFilter::All, NodeRole::Writer, true),
        (RoleFilter::All, NodeRole::Replica, true),
        (RoleFilter::Writer, NodeRole::Writer, true),
        (RoleFilter::Writer, NodeRole::Replica, false),
        (RoleFilter::Replica, NodeRole::Replica, true),
        (RoleFilter::Replica, NodeRole::Writer, false),
    ] {
        assert_eq!(filter.matches(role), expected);
    }
}
