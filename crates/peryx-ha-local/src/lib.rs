//! Single-process availability with no workers, timers, listeners, or mutable state.

use peryx_core::{LocalNode, NodeLiveness, NodeRole, TopologyConfig, TopologyMode, TopologySnapshot};
use peryx_ha::{HaCoordinator, Lease, MembershipProvider};

/// Disabled distributed availability.
#[derive(Debug, Default, Clone, Copy)]
pub struct Local;

impl MembershipProvider for Local {
    fn members(&self) -> &[peryx_core::TopologyMember] {
        &[]
    }
}

impl Lease for Local {
    fn holder(&self) -> Option<&str> {
        None
    }
}

impl HaCoordinator for Local {
    fn configuration(&self) -> TopologyConfig {
        TopologyConfig {
            mode: TopologyMode::None,
            group: None,
            members: Vec::new(),
            local_node: None,
        }
    }

    fn topology(&self, captured_at: i64) -> TopologySnapshot {
        TopologySnapshot {
            mode: TopologyMode::None,
            group: None,
            captured_at,
            node_count: 1,
            local: LocalNode {
                role: NodeRole::Writer,
                liveness: Some(NodeLiveness::Live),
                frontier: None,
            },
            nodes: Vec::new(),
        }
    }

    fn distributed(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use peryx_core::{NodeLiveness, NodeRole, TopologyMode};
    use peryx_ha::{HaCoordinator as _, Lease as _, MembershipProvider as _};

    use super::Local;

    #[test]
    fn test_configuration_is_single_node() {
        let config = Local.configuration();

        assert_eq!(config.mode, TopologyMode::None);
        assert!(config.group.is_none());
        assert!(config.members.is_empty());
        assert!(config.local_node.is_none());
    }

    #[test]
    fn test_topology_is_immediately_live() {
        let topology = Local.topology(42);

        assert_eq!(topology.mode, TopologyMode::None);
        assert_eq!(topology.captured_at, 42);
        assert_eq!(topology.node_count, 1);
        assert_eq!(topology.local.role, NodeRole::Writer);
        assert_eq!(topology.local.liveness, Some(NodeLiveness::Live));
        assert!(topology.nodes.is_empty());
    }

    #[test]
    fn test_local_has_no_members() {
        assert!(Local.members().is_empty());
    }

    #[test]
    fn test_local_has_no_lease_holder() {
        assert!(Local.holder().is_none());
    }

    #[test]
    fn test_local_is_not_distributed() {
        assert!(!Local.distributed());
    }
}
