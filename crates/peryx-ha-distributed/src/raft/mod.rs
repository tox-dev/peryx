//! Each voter uses a `u64` Raft node ID; [`PeryxNode`] carries its datacenter identity and RPC endpoint.

use std::io::Cursor;

use serde::{Deserialize, Serialize};

use crate::ownership::{DatacenterId, OwnershipCommand, OwnershipEffect};

mod config;
pub mod log_store;
pub mod network;
mod node;
pub(crate) mod persistence;
mod state_machine;

pub use config::RaftConfig;
pub use node::{RaftNode, StartError};
pub use state_machine::OwnershipStateMachine;

#[cfg(test)]
#[path = "../../tests/unit/raft/config_tests.rs"]
mod config_tests;
#[cfg(test)]
#[path = "../../tests/unit/raft/node_tests.rs"]
mod node_tests;
#[cfg(test)]
#[path = "../../tests/unit/raft/persistence_tests.rs"]
mod persistence_tests;
#[cfg(test)]
#[path = "../../tests/unit/raft/state_machine_tests.rs"]
mod state_machine_tests;

openraft::declare_raft_types!(
    pub TypeConfig:
        D = OwnershipCommand,
        R = OwnershipResponse,
        Node = PeryxNode,
);

#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PeryxNode {
    pub datacenter: DatacenterId,
    /// The peer's canonical base URL, scheme included. `OpenRaft` requires [`Default`] on node data, so
    /// this holds the rendering of a [`MemberEndpoint`](peryx_ha::MemberEndpoint) rather than the type.
    pub endpoint: String,
}

/// Blank and membership entries return [`NonMutating`](Self::NonMutating) because they carry no
/// [`OwnershipCommand`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OwnershipResponse {
    Applied(OwnershipEffect),
    NonMutating,
}
