//! Epoch zero means unassigned. Assignment starts at one; advances and transfers increase the epoch.
//! Invalid transitions leave the state unchanged, so all replicas derive the same state from the same
//! command sequence.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::authority::AuthorityKey;
use crate::envelope::AuthorityEpoch;

/// The unassigned epoch, which [`AuthorityFence`](crate::AuthorityFence) rejects.
const UNASSIGNED: AuthorityEpoch = AuthorityEpoch(0);

#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DatacenterId(pub String);

/// Raft term and index retained with an assignment for audit and replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppliedMeta {
    pub term: u64,
    pub index: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AssignmentCause {
    FirstPublish,
}

/// Assignment provenance persisted in ownership snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assignment {
    pub cause: AssignmentCause,
    pub term: u64,
    pub index: u64,
    /// Always epoch one, before any advance or transfer.
    pub epoch: AuthorityEpoch,
}

/// Transfer provenance used by reconciliation and drain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferRecord {
    pub from: DatacenterId,
    pub to: DatacenterId,
    pub epoch: AuthorityEpoch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OwnershipCommand {
    /// Assigns an unowned authority at epoch one.
    AssignHome {
        authority: AuthorityKey,
        home: DatacenterId,
        cause: AssignmentCause,
    },
    /// Increases an assigned authority's epoch without moving its home.
    AdvanceAuthorityEpoch { authority: AuthorityKey },
    /// Moves an assigned authority, increases its epoch, and records the transfer.
    RecordTransfer {
        authority: AuthorityKey,
        new_home: DatacenterId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OwnershipEffect {
    Assigned {
        epoch: AuthorityEpoch,
    },
    EpochAdvanced {
        epoch: AuthorityEpoch,
    },
    Transferred {
        from: DatacenterId,
        to: DatacenterId,
        epoch: AuthorityEpoch,
    },
    /// The command was invalid and left ownership unchanged.
    Rejected(Rejection),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Rejection {
    AlreadyAssigned,
    NotAssigned,
    SameHome,
}

#[derive(Debug, thiserror::Error)]
pub enum OwnershipError {
    #[error("ownership snapshot is malformed: {0}")]
    Malformed(#[source] serde_json::Error),
    #[error("ownership snapshot homes authority {authority:?} at the reserved zero epoch")]
    ZeroEpoch { authority: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AuthorityRecord {
    home: DatacenterId,
    epoch: AuthorityEpoch,
    assignment: Assignment,
    transfers: Vec<TransferRecord>,
}

/// Missing authorities are unassigned and read as epoch zero.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OwnershipState {
    authorities: BTreeMap<String, AuthorityRecord>,
}

impl OwnershipState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies a committed command. Invalid commands leave state unchanged; assignments retain `meta`.
    pub fn apply(&mut self, command: &OwnershipCommand, meta: AppliedMeta) -> OwnershipEffect {
        match command {
            OwnershipCommand::AssignHome { authority, home, cause } => self.assign_home(authority, home, *cause, meta),
            OwnershipCommand::AdvanceAuthorityEpoch { authority } => self.advance_epoch(authority),
            OwnershipCommand::RecordTransfer { authority, new_home } => self.transfer(authority, new_home),
        }
    }

    fn assign_home(
        &mut self,
        authority: &AuthorityKey,
        home: &DatacenterId,
        cause: AssignmentCause,
        meta: AppliedMeta,
    ) -> OwnershipEffect {
        if self.authorities.contains_key(&authority.0) {
            return OwnershipEffect::Rejected(Rejection::AlreadyAssigned);
        }
        let epoch = AuthorityEpoch(1);
        self.authorities.insert(
            authority.0.clone(),
            AuthorityRecord {
                home: home.clone(),
                epoch,
                assignment: Assignment {
                    cause,
                    term: meta.term,
                    index: meta.index,
                    epoch,
                },
                transfers: Vec::new(),
            },
        );
        OwnershipEffect::Assigned { epoch }
    }

    fn advance_epoch(&mut self, authority: &AuthorityKey) -> OwnershipEffect {
        let Some(record) = self.authorities.get_mut(&authority.0) else {
            return OwnershipEffect::Rejected(Rejection::NotAssigned);
        };
        let epoch = AuthorityEpoch(record.epoch.0 + 1);
        record.epoch = epoch;
        OwnershipEffect::EpochAdvanced { epoch }
    }

    fn transfer(&mut self, authority: &AuthorityKey, new_home: &DatacenterId) -> OwnershipEffect {
        let Some(record) = self.authorities.get_mut(&authority.0) else {
            return OwnershipEffect::Rejected(Rejection::NotAssigned);
        };
        if record.home == *new_home {
            return OwnershipEffect::Rejected(Rejection::SameHome);
        }
        let epoch = AuthorityEpoch(record.epoch.0 + 1);
        let from = std::mem::replace(&mut record.home, new_home.clone());
        record.epoch = epoch;
        record.transfers.push(TransferRecord {
            from: from.clone(),
            to: new_home.clone(),
            epoch,
        });
        OwnershipEffect::Transferred {
            from,
            to: new_home.clone(),
            epoch,
        }
    }

    /// Returns epoch zero when `authority` is unassigned.
    #[must_use]
    pub fn epoch(&self, authority: &AuthorityKey) -> AuthorityEpoch {
        self.authorities
            .get(&authority.0)
            .map_or(UNASSIGNED, |record| record.epoch)
    }

    #[must_use]
    pub fn home(&self, authority: &AuthorityKey) -> Option<&DatacenterId> {
        self.authorities.get(&authority.0).map(|record| &record.home)
    }

    #[must_use]
    pub fn assignment(&self, authority: &AuthorityKey) -> Option<&Assignment> {
        self.authorities.get(&authority.0).map(|record| &record.assignment)
    }

    /// Returns transfers oldest first, or an empty slice for an unassigned or unmoved authority.
    #[must_use]
    pub fn transfers(&self, authority: &AuthorityKey) -> &[TransferRecord] {
        self.authorities
            .get(&authority.0)
            .map_or(&[], |record| record.transfers.as_slice())
    }

    /// # Panics
    /// JSON serialization failure, which the state's field types make unreachable.
    #[must_use]
    pub fn snapshot(&self) -> Vec<u8> {
        serde_json::to_vec(&self.authorities).expect("an ownership state always serializes to JSON")
    }

    /// Rejects homed authorities at epoch zero because the fence reserves zero for unassigned state.
    ///
    /// # Errors
    /// [`OwnershipError::Malformed`] when the bytes are not a valid snapshot, or
    /// [`OwnershipError::ZeroEpoch`] when a homed authority carries the reserved zero epoch.
    pub fn restore(bytes: &[u8]) -> Result<Self, OwnershipError> {
        let authorities: BTreeMap<String, AuthorityRecord> =
            serde_json::from_slice(bytes).map_err(OwnershipError::Malformed)?;
        for (authority, record) in &authorities {
            if record.epoch == UNASSIGNED {
                return Err(OwnershipError::ZeroEpoch {
                    authority: authority.clone(),
                });
            }
        }
        Ok(Self { authorities })
    }
}
