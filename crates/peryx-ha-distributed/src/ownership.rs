//! The replicated ownership state machine: which datacenter homes each authority, the authority epoch
//! that fences its work, and the transfer trail behind it.
//!
//! Home authority moves between datacenters - a project or repository is first homed where it is
//! published, and a failover transfers it to a surviving datacenter. Every replica must agree on who
//! owns an authority and under which epoch, or two datacenters could both accept writes for it. This is
//! the deterministic state behind that agreement: [`apply`](OwnershipState::apply) folds one committed
//! [`OwnershipCommand`] into the state, and the same command over the same state produces the same
//! state on every voter, so the Raft log that orders the commands is the only coordination needed.
//!
//! It is the producer half of the authority-epoch fence. [`AuthorityFence`](crate::AuthorityFence)
//! consumes committed epochs and admits only the work that carries the current one; this mints them.
//! Epochs are one-based and strictly increasing: an authority no command has homed reads as
//! [`AuthorityEpoch(0)`](crate::AuthorityEpoch) - the same unassigned sentinel the fence fails closed
//! on - and the first assignment mints epoch one, so a real epoch is never zero and the fence can trust
//! a nonzero epoch as an owned authority.
//!
//! The fold is pure and total: a command and the current state decide the next state, with no clock,
//! network, or storage. An invalid transition - assigning an already-owned authority, advancing or
//! transferring one that was never assigned, or transferring to the home it already holds - is rejected
//! without touching the state, so a replayed or malformed command cannot corrupt ownership.
//!
//! Wiring this behind a Raft state machine that applies committed commands, and driving the minted
//! epochs into [`AuthorityFence`](crate::AuthorityFence), is deferred integration; this module only
//! decides the state.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::authority::AuthorityKey;
use crate::envelope::AuthorityEpoch;

/// The unassigned-authority sentinel: an authority no command has homed reads as this epoch, the value
/// [`AuthorityFence`](crate::AuthorityFence) fences all work on.
const UNASSIGNED: AuthorityEpoch = AuthorityEpoch(0);

/// A datacenter that can hold authority over an artifact home.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DatacenterId(pub String);

/// The committed-entry provenance the state machine reads off each applied log entry: the leader term
/// and log index of the [`OwnershipCommand`] that produced a transition.
///
/// The Raft log position `(term, index)` is the operation's identity - unique and totally ordered - so
/// recording it on an assignment ties the home to the exact committed command for audit and replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppliedMeta {
    pub term: u64,
    pub index: u64,
}

/// Why an authority was homed. First publish is the only cause today; preassignment and operator
/// transfer are out of scope, so the enum names the one path that assigns a home.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AssignmentCause {
    /// The first successful publish to a project or repository homed it where it was published.
    FirstPublish,
}

/// The audit trail of a home assignment: why it happened, the committed command that carried it, and the
/// epoch it minted. Persisted in the snapshot so a replay reconstructs the same provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assignment {
    pub cause: AssignmentCause,
    pub term: u64,
    pub index: u64,
    /// The epoch the assignment minted, always [`AuthorityEpoch(1)`](crate::AuthorityEpoch) - the first
    /// epoch of a freshly homed authority, before any advance or transfer.
    pub epoch: AuthorityEpoch,
}

/// One authority transfer: the datacenters it moved between and the epoch it minted. The trail of these
/// is the audit record reconciliation and drain read to classify operations left by the prior home.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferRecord {
    pub from: DatacenterId,
    pub to: DatacenterId,
    pub epoch: AuthorityEpoch,
}

/// A committed instruction to the ownership state machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OwnershipCommand {
    /// Home a previously unassigned authority, minting its first epoch. The first-publish path.
    AssignHome {
        authority: AuthorityKey,
        home: DatacenterId,
        cause: AssignmentCause,
    },
    /// Mint the next epoch for an assigned authority without moving its home, fencing every operation
    /// under the prior epoch. Fencing a stale home that rejoined uses this.
    AdvanceAuthorityEpoch { authority: AuthorityKey },
    /// Move an assigned authority to a new home, minting the next epoch and appending a transfer
    /// record. Failover transfers authority this way.
    RecordTransfer {
        authority: AuthorityKey,
        new_home: DatacenterId,
    },
}

/// What [`apply`](OwnershipState::apply) made of a command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OwnershipEffect {
    /// A home was assigned to a previously unassigned authority at the minted epoch.
    Assigned { epoch: AuthorityEpoch },
    /// An assigned authority's epoch advanced, fencing prior-epoch work; its home is unchanged.
    EpochAdvanced { epoch: AuthorityEpoch },
    /// An authority moved to a new home at the minted epoch.
    Transferred {
        from: DatacenterId,
        to: DatacenterId,
        epoch: AuthorityEpoch,
    },
    /// The command was invalid against the current state, which is left unchanged.
    Rejected(Rejection),
}

/// Why the state machine rejected a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Rejection {
    /// [`AssignHome`](OwnershipCommand::AssignHome) named an authority that already has a home; a homed
    /// authority moves only through [`RecordTransfer`](OwnershipCommand::RecordTransfer).
    AlreadyAssigned,
    /// [`AdvanceAuthorityEpoch`](OwnershipCommand::AdvanceAuthorityEpoch) or
    /// [`RecordTransfer`](OwnershipCommand::RecordTransfer) named an authority with no home; it must be
    /// assigned first.
    NotAssigned,
    /// [`RecordTransfer`](OwnershipCommand::RecordTransfer) named the home the authority already holds; a
    /// transfer must move it.
    SameHome,
}

/// Restoring an ownership state from a snapshot failed.
#[derive(Debug, thiserror::Error)]
pub enum OwnershipError {
    #[error("ownership snapshot is malformed: {0}")]
    Malformed(#[source] serde_json::Error),
    #[error("ownership snapshot homes authority {authority:?} at the reserved zero epoch")]
    ZeroEpoch { authority: String },
}

/// One authority's ownership: its current home, current epoch, and the transfers that led here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AuthorityRecord {
    home: DatacenterId,
    epoch: AuthorityEpoch,
    assignment: Assignment,
    transfers: Vec<TransferRecord>,
}

/// The home, epoch, and transfer trail of every authority a stream of commands has assigned.
///
/// Keyed by authority so distinct authorities move independently; an authority absent from the map is
/// unassigned and reads as the [`AuthorityEpoch(0)`](crate::AuthorityEpoch) sentinel.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OwnershipState {
    authorities: BTreeMap<String, AuthorityRecord>,
}

impl OwnershipState {
    /// An empty state, in which every authority is unassigned.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply `command` committed at `meta`, returning the transition it produced or why it was rejected.
    ///
    /// The state advances only on a valid transition; a rejected command leaves it untouched, so a
    /// replayed or invalid command is a deterministic no-op rather than a corruption. `meta` carries the
    /// committed log position, which an assignment records for audit; the other transitions do not read
    /// it, so their outcome depends on the command and current state alone.
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

    /// The current epoch of `authority`, or the [`AuthorityEpoch(0)`](crate::AuthorityEpoch) unassigned
    /// sentinel when no command has homed it - the value to hand [`AuthorityFence`](crate::AuthorityFence).
    #[must_use]
    pub fn epoch(&self, authority: &AuthorityKey) -> AuthorityEpoch {
        self.authorities
            .get(&authority.0)
            .map_or(UNASSIGNED, |record| record.epoch)
    }

    /// The current home of `authority`, or `None` when no command has homed it.
    #[must_use]
    pub fn home(&self, authority: &AuthorityKey) -> Option<&DatacenterId> {
        self.authorities.get(&authority.0).map(|record| &record.home)
    }

    /// The assignment audit of `authority` - its cause, committed log position, and minted epoch - or
    /// `None` when no command has homed it. The trail a replay or an operator reads to see where and how
    /// a home was first assigned.
    #[must_use]
    pub fn assignment(&self, authority: &AuthorityKey) -> Option<&Assignment> {
        self.authorities.get(&authority.0).map(|record| &record.assignment)
    }

    /// The transfers `authority` has been through, oldest first, or an empty slice when it was never
    /// assigned or never moved.
    #[must_use]
    pub fn transfers(&self, authority: &AuthorityKey) -> &[TransferRecord] {
        self.authorities
            .get(&authority.0)
            .map_or(&[], |record| record.transfers.as_slice())
    }

    /// Serialize the full state to its snapshot bytes.
    ///
    /// # Panics
    /// Panics only if serializing to JSON fails, which the state's field types make unreachable.
    #[must_use]
    pub fn snapshot(&self) -> Vec<u8> {
        serde_json::to_vec(&self.authorities).expect("an ownership state always serializes to JSON")
    }

    /// Restore a state from [`snapshot`](OwnershipState::snapshot) bytes.
    ///
    /// Every homed authority must carry a nonzero epoch: zero is the unassigned sentinel, so a record at
    /// epoch zero would read as unassigned and let the fence admit work it should hold, which restore
    /// rejects rather than load.
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
