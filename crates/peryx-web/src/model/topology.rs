//! Presentation helpers over [`peryx_core::TopologySnapshot`], the availability-topology DTO the
//! server projects and the browser deserializes unchanged. The snapshot already carries only the
//! fields the caller's class admits, so these helpers add labels and a role filter, never authority.

use peryx_core::{NodeLiveness, NodeRole, TopologyMode};

pub use peryx_core::{LocalNode, TopologyNode, TopologySnapshot};

/// A health cell: the text an operator reads plus the css class that tints it. The text stands on its
/// own so a color-blind reader loses nothing, and a withheld field reads as restricted, never healthy.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HealthLabel {
    pub text: &'static str,
    pub class: &'static str,
}

/// A node's liveness as a labelled health cell. `None` means the caller's class did not admit the
/// field, so it reads `restricted` rather than borrowing a healthy look it was not granted.
#[must_use]
pub const fn liveness_health(liveness: Option<NodeLiveness>) -> HealthLabel {
    match liveness {
        Some(NodeLiveness::Live) => HealthLabel {
            text: "Live",
            class: "health-live",
        },
        Some(NodeLiveness::Unready) => HealthLabel {
            text: "Unready",
            class: "health-unready",
        },
        Some(NodeLiveness::Unknown) => HealthLabel {
            text: "Unknown",
            class: "health-unknown",
        },
        None => HealthLabel {
            text: "Restricted",
            class: "health-restricted",
        },
    }
}

/// The live-stream connection state, shown beside the snapshot so a paused feed never passes for fresh.
///
/// A feed starts `Connecting` and only turns `Live` once the connection opens or a valid event arrives, so
/// it never claims to be live while the browser is still connecting. `Connecting` also covers every
/// automatic reconnect; `Stale` means the connection is open but sent data the browser could not decode,
/// freezing the render behind a protocol error; `Offline` means the browser gave up.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum StreamStatus {
    Live,
    #[default]
    Connecting,
    Stale,
    Offline,
}

/// The connection state as a labelled health cell, reusing the roster's health palette so an operator
/// reads the feed's health the same way as a node's. A frozen feed never borrows the `Live` tint.
#[must_use]
pub const fn stream_status_label(status: StreamStatus) -> HealthLabel {
    match status {
        StreamStatus::Live => HealthLabel {
            text: "Live",
            class: "health-live",
        },
        StreamStatus::Connecting => HealthLabel {
            text: "Reconnecting",
            class: "health-unready",
        },
        StreamStatus::Stale => HealthLabel {
            text: "Stale",
            class: "health-unready",
        },
        StreamStatus::Offline => HealthLabel {
            text: "Offline",
            class: "health-unknown",
        },
    }
}

#[must_use]
pub const fn role_label(role: NodeRole) -> &'static str {
    match role {
        NodeRole::Writer => "Writer",
        NodeRole::Replica => "Replica",
    }
}

#[must_use]
pub const fn mode_label(mode: TopologyMode) -> &'static str {
    match mode {
        TopologyMode::None => "Single node",
        TopologyMode::Dc => "Datacenter",
        TopologyMode::Ha => "High availability",
    }
}

/// The roster filter a reader drives from the role select. `All` is the default so the accessible,
/// script-free render shows the whole roster.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum RoleFilter {
    #[default]
    All,
    Writer,
    Replica,
}

impl RoleFilter {
    #[must_use]
    pub fn from_value(value: &str) -> Self {
        match value {
            "writer" => Self::Writer,
            "replica" => Self::Replica,
            _ => Self::All,
        }
    }

    #[must_use]
    pub fn matches(self, role: NodeRole) -> bool {
        match self {
            Self::All => true,
            Self::Writer => role == NodeRole::Writer,
            Self::Replica => role == NodeRole::Replica,
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/model/topology/tests.rs"]
mod tests;
