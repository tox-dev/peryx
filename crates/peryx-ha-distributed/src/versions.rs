//! Deciding whether two nodes' advertised availability versions can interoperate.
//!
//! Rolling an availability cluster through an upgrade mixes nodes at different builds, so before they
//! exchange commands they settle on one protocol and one state-machine version both speak, and hold a
//! new command back until every committed member can apply it. This module is that agreement as pure
//! decisions over advertised versions: [`negotiate`] picks the shared versions or names the dimension
//! that cannot agree, [`feature_activated`] gates a command on the whole membership, and
//! [`snapshot_compatible`] and [`accepts_operation_kind`] guard one incoming artifact. Versions and
//! kinds are inputs, so nothing here reaches for transport, the Raft loop, a clock, or storage;
//! advertising these values and committing them through the state machine is the integration that
//! consumes the module.

/// A version of one availability dimension, the wire protocol or the replicated state machine. Higher
/// is newer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u16);

/// The inclusive span of versions a node supports for one dimension, from its oldest still-supported
/// `min` to its newest `max`. A node speaks every version in `min..=max`, so callers keep `min <= max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionRange {
    pub min: Version,
    pub max: Version,
}

impl VersionRange {
    /// Whether `version` falls within this supported span.
    #[must_use]
    pub fn supports(self, version: Version) -> bool {
        self.min <= version && version <= self.max
    }
}

/// A node's advertised support for both availability dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AvailabilityVersions {
    pub protocol: VersionRange,
    pub state_machine: VersionRange,
}

/// One operation kind as it arrives on the wire: its stable discriminant and whether a receiver must
/// understand it to stay consistent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireKind {
    pub discriminant: u16,
    pub required: bool,
}

/// The dimension whose ranges do not overlap, so two nodes cannot settle on a version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Incompatibility {
    Protocol { local: VersionRange, peer: VersionRange },
    StateMachine { local: VersionRange, peer: VersionRange },
}

/// The outcome of negotiating two nodes' advertised versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Negotiation {
    /// Both dimensions overlap; each names the highest version the two nodes share.
    Compatible { protocol: Version, state_machine: Version },
    /// One dimension has no shared version, so the nodes cannot interoperate.
    Incompatible(Incompatibility),
}

/// Negotiate the versions two nodes both support.
///
/// Each dimension resolves to the highest shared version so an upgraded pair uses its newest common
/// capability. Protocol is decided first; a dimension whose ranges do not overlap yields
/// [`Incompatible`](Negotiation::Incompatible) naming it.
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

/// Whether a feature introduced at state-machine version `floor` is active across `membership`.
///
/// It stays off until every committed member can apply it, and off for an empty membership, so a
/// partly upgraded cluster never issues a command an older member cannot apply.
#[must_use]
pub fn feature_activated(floor: Version, membership: &[AvailabilityVersions]) -> bool {
    !membership.is_empty() && membership.iter().all(|node| node.state_machine.max >= floor)
}

/// Whether a build that understands the `known` discriminants may apply an operation of `kind`.
///
/// A known kind always applies; an unknown one applies only when the sender marked it ignorable, so an
/// unknown required kind fails closed.
#[must_use]
pub fn accepts_operation_kind(known: &[u16], kind: WireKind) -> bool {
    known.contains(&kind.discriminant) || !kind.required
}

/// Whether a node advertising the `local` state-machine range can restore a snapshot at that version.
///
/// It holds only while `snapshot_version` stays within the range the node still supports.
#[must_use]
pub fn snapshot_compatible(snapshot_version: Version, local: VersionRange) -> bool {
    local.supports(snapshot_version)
}

/// The highest version both ranges support, or `None` when they do not overlap.
fn highest_shared(left: VersionRange, right: VersionRange) -> Option<Version> {
    let low = left.min.max(right.min);
    let high = left.max.min(right.max);
    (low <= high).then_some(high)
}
