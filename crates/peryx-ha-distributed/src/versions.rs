//! Compatibility rules for rolling availability upgrades.
//!
//! Negotiation selects the highest shared protocol and state-machine versions. A feature remains
//! disabled until every committed member supports it.

/// A protocol or state-machine version; higher values are newer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u16);

/// Inclusive supported span. Callers must maintain `min <= max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionRange {
    pub min: Version,
    pub max: Version,
}

impl VersionRange {
    #[must_use]
    pub fn supports(self, version: Version) -> bool {
        self.min <= version && version <= self.max
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AvailabilityVersions {
    pub protocol: VersionRange,
    pub state_machine: VersionRange,
}

/// Wire operation kind with a stable discriminant and consistency requirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireKind {
    pub discriminant: u16,
    pub required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Incompatibility {
    Protocol { local: VersionRange, peer: VersionRange },
    StateMachine { local: VersionRange, peer: VersionRange },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Negotiation {
    Compatible { protocol: Version, state_machine: Version },
    Incompatible(Incompatibility),
}

/// Selects the highest shared version in each dimension. Protocol incompatibility takes precedence
/// over state-machine incompatibility.
#[must_use]
pub fn negotiate(local: &AvailabilityVersions, peer: &AvailabilityVersions) -> Negotiation {
    let Some(protocol) = highest_shared(local.protocol, peer.protocol) else {
        return Negotiation::Incompatible(Incompatibility::Protocol {
            local: local.protocol,
            peer: peer.protocol,
        });
    };
    let Some(state_machine) = highest_shared(local.state_machine, peer.state_machine) else {
        return Negotiation::Incompatible(Incompatibility::StateMachine {
            local: local.state_machine,
            peer: peer.state_machine,
        });
    };
    Negotiation::Compatible {
        protocol,
        state_machine,
    }
}

/// Activates a feature when each committed member supports `floor`; an empty membership does not.
#[must_use]
pub fn feature_activated(floor: Version, membership: &[AvailabilityVersions]) -> bool {
    !membership.is_empty() && membership.iter().all(|node| node.state_machine.max >= floor)
}

/// Accepts known kinds and unknown optional kinds; rejects unknown required kinds.
#[must_use]
pub fn accepts_operation_kind(known: &[u16], kind: WireKind) -> bool {
    known.contains(&kind.discriminant) || !kind.required
}

/// Accepts snapshots within the local supported state-machine range.
#[must_use]
pub fn snapshot_compatible(snapshot_version: Version, local: VersionRange) -> bool {
    local.supports(snapshot_version)
}

fn highest_shared(left: VersionRange, right: VersionRange) -> Option<Version> {
    let low = left.min.max(right.min);
    let high = left.max.min(right.max);
    (low <= high).then_some(high)
}
