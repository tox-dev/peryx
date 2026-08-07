//! The `OpenRaft` type configuration for the ownership consensus group.
//!
//! HA runs the ownership state machine ([`OwnershipState`](crate::OwnershipState)) as a replicated
//! [OpenRaft](https://docs.rs/openraft) log: each committed [`OwnershipCommand`] is one log entry, and
//! every voter applies the same committed entries in the same order to reach the same state. This
//! module binds peryx's types to `OpenRaft` through [`TypeConfig`], the single type map the log storage,
//! network, and state-machine adapters all build against.
//!
//! A voter is a datacenter. `OpenRaft` identifies a node by a `Copy` [`NodeId`](TypeConfig), so the
//! numeric `u64` names the voter and [`PeryxNode`] carries the datacenter identity and the address the
//! network adapter dials. The log payload is [`OwnershipCommand`] and the apply result is
//! [`OwnershipResponse`], which wraps the [`OwnershipEffect`] of a command and marks the blank and
//! membership entries `OpenRaft` commits on its own as non-mutating.

use std::io::Cursor;

use serde::{Deserialize, Serialize};

use crate::ownership::{DatacenterId, OwnershipCommand, OwnershipEffect};

mod config;
pub mod log_store;
pub mod network;
mod node;
mod state_machine;

pub use config::RaftConfig;
pub use node::{RaftNode, StartError};
pub use state_machine::OwnershipStateMachine;

#[cfg(test)]
mod config_tests;
#[cfg(test)]
mod node_tests;
#[cfg(test)]
mod state_machine_tests;

openraft::declare_raft_types!(
    /// The type map binding peryx's ownership types to `OpenRaft`.
    ///
    /// `D` is the committed log payload ([`OwnershipCommand`]) and `R` the apply result
    /// ([`OwnershipResponse`]). The node id is a `u64` voter handle and the node data is [`PeryxNode`].
    /// Entry, snapshot data, async runtime, and responder take `OpenRaft`'s defaults: an
    /// `Entry<TypeConfig>` log entry, a `Cursor<Vec<u8>>` snapshot the state machine fills from
    /// [`OwnershipState::snapshot`](crate::OwnershipState::snapshot), the Tokio runtime, and the
    /// one-shot responder.
    pub TypeConfig:
        D = OwnershipCommand,
        R = OwnershipResponse,
        Node = PeryxNode,
);

/// A voting datacenter in the ownership consensus group: its datacenter identity and the address the
/// network adapter dials to reach it.
///
/// `OpenRaft` keys a node by the `Copy` `u64` [`NodeId`](TypeConfig); this is the per-node data it
/// carries alongside that id. The `datacenter` maps a voter back to the [`DatacenterId`] the ownership
/// commands name, and `addr` is where the Raft RPCs go.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PeryxNode {
    pub datacenter: DatacenterId,
    pub addr: String,
}

/// The result of applying one committed log entry.
///
/// A [`Normal`](openraft::EntryPayload::Normal) entry carries an [`OwnershipCommand`], so its result is
/// the command's [`OwnershipEffect`]. The blank entry a new leader commits and the membership entries
/// `OpenRaft` manages carry no ownership command, so they apply as [`NonMutating`](Self::NonMutating).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OwnershipResponse {
    /// The entry carried an [`OwnershipCommand`]; this is the effect of applying it.
    Applied(OwnershipEffect),
    /// A blank or membership entry that carried no ownership command.
    NonMutating,
}
