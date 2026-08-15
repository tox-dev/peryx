use crate::versions::{
    AvailabilityVersions, Incompatibility, Negotiation, Version, VersionRange, WireKind, accepts_operation_kind,
    feature_activated, negotiate, snapshot_compatible,
};

fn v(number: u16) -> Version {
    Version(number)
}

fn range(min: u16, max: u16) -> VersionRange {
    VersionRange {
        min: Version(min),
        max: Version(max),
    }
}

fn versions(protocol: (u16, u16), state_machine: (u16, u16)) -> AvailabilityVersions {
    AvailabilityVersions {
        protocol: range(protocol.0, protocol.1),
        state_machine: range(state_machine.0, state_machine.1),
    }
}

#[test]
fn test_negotiate_picks_the_highest_shared_version_in_each_dimension() {
    let local = versions((1, 3), (1, 2));
    let peer = versions((2, 5), (1, 4));

    assert_eq!(
        negotiate(&local, &peer),
        Negotiation::Compatible {
            protocol: v(3),
            state_machine: v(2),
        },
        "the negotiated version is the top of the overlap, not merely a shared version",
    );
}

#[test]
fn test_negotiate_reports_the_dimension_whose_ranges_do_not_overlap() {
    for (local, peer, expected) in [
        (
            versions((1, 2), (1, 4)),
            versions((4, 5), (1, 4)),
            Incompatibility::Protocol {
                local: range(1, 2),
                peer: range(4, 5),
            },
        ),
        (
            versions((1, 3), (1, 2)),
            versions((1, 3), (4, 5)),
            Incompatibility::StateMachine {
                local: range(1, 2),
                peer: range(4, 5),
            },
        ),
    ] {
        assert_eq!(
            negotiate(&local, &peer),
            Negotiation::Incompatible(expected),
            "{expected:?}"
        );
    }
}

#[test]
fn test_feature_activates_only_when_every_member_meets_the_floor() {
    let floor = v(3);
    for (membership, expected, note) in [
        (Vec::new(), false, "an empty membership fails closed"),
        (
            vec![versions((1, 1), (1, 3)), versions((1, 1), (1, 3))],
            true,
            "every member sits exactly at the floor",
        ),
        (
            vec![versions((1, 1), (1, 4)), versions((1, 1), (1, 3))],
            true,
            "every member is at or past the floor",
        ),
        (
            vec![versions((1, 1), (1, 4)), versions((1, 1), (1, 2))],
            false,
            "one member lags the floor",
        ),
    ] {
        assert_eq!(feature_activated(floor, &membership), expected, "{note}");
    }
}

#[test]
fn test_accepts_operation_kind_fails_closed_only_on_an_unknown_required_kind() {
    let known = [1_u16, 2, 3];
    for (kind, expected, note) in [
        (
            WireKind {
                discriminant: 2,
                required: true,
            },
            true,
            "a known required kind",
        ),
        (
            WireKind {
                discriminant: 2,
                required: false,
            },
            true,
            "a known optional kind",
        ),
        (
            WireKind {
                discriminant: 9,
                required: false,
            },
            true,
            "an unknown ignorable kind",
        ),
        (
            WireKind {
                discriminant: 9,
                required: true,
            },
            false,
            "an unknown required kind",
        ),
    ] {
        assert_eq!(accepts_operation_kind(&known, kind), expected, "{note}");
    }
}

#[test]
fn test_snapshot_compatible_within_the_supported_range_inclusive() {
    let local = range(2, 5);
    for (snapshot, expected) in [(1, false), (2, true), (3, true), (5, true), (6, false)] {
        assert_eq!(
            snapshot_compatible(v(snapshot), local),
            expected,
            "snapshot v{snapshot} against {local:?}"
        );
    }
}
