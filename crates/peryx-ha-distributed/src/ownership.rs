//! Epoch zero means unassigned. Assignment starts at one; advances and transfers increase the epoch.
//! Invalid transitions leave the state unchanged, so all replicas derive the same state from the same
//! command sequence.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::authority::AuthorityKey;
use crate::envelope::AuthorityEpoch;
use peryx_ha::{AUTHORITY_CLOCK_SKEW_SECS, AUTHORITY_WRITE_LEASE_SECS};

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
    AdvanceAuthorityEpoch { authority: AuthorityKey, now_unix: i64 },
    BeginEpochWrite {
        authority: AuthorityKey,
        epoch: AuthorityEpoch,
        id: String,
        issued_at_unix: i64,
        expires_at_unix: i64,
    },
    FinishEpochWrite {
        authority: AuthorityKey,
        epoch: AuthorityEpoch,
        id: String,
    },
    /// Moves an assigned authority, increases its epoch, and records the transfer.
    RecordTransfer {
        authority: AuthorityKey,
        new_home: DatacenterId,
        now_unix: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OwnershipEffect {
    Assigned {
        home: DatacenterId,
        epoch: AuthorityEpoch,
    },
    AlreadyAssigned {
        home: DatacenterId,
        epoch: AuthorityEpoch,
    },
    EpochAdvanced {
        epoch: AuthorityEpoch,
    },
    WriteLeased {
        epoch: AuthorityEpoch,
        id: String,
        expires_at_unix: i64,
    },
    WriteFinished,
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
    NotAssigned,
    SameHome,
    EpochMismatch,
    InvalidLease,
    WritesInFlight,
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
    #[serde(default)]
    writes: BTreeMap<String, WriteLeaseRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WriteLeaseRecord {
    epoch: AuthorityEpoch,
    expires_at_unix: i64,
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
            OwnershipCommand::AdvanceAuthorityEpoch { authority, now_unix } => self.advance_epoch(authority, *now_unix),
            OwnershipCommand::BeginEpochWrite {
                authority,
                epoch,
                id,
                issued_at_unix,
                expires_at_unix,
            } => self.begin_write(authority, *epoch, id, *issued_at_unix, *expires_at_unix),
            OwnershipCommand::FinishEpochWrite { authority, epoch, id } => self.finish_write(authority, *epoch, id),
            OwnershipCommand::RecordTransfer {
                authority,
                new_home,
                now_unix,
            } => self.transfer(authority, new_home, *now_unix),
        }
    }

    fn assign_home(
        &mut self,
        authority: &AuthorityKey,
        home: &DatacenterId,
        cause: AssignmentCause,
        meta: AppliedMeta,
    ) -> OwnershipEffect {
        if let Some(record) = self.authorities.get(&authority.0) {
            return OwnershipEffect::AlreadyAssigned {
                home: record.home.clone(),
                epoch: record.epoch,
            };
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
                writes: BTreeMap::new(),
            },
        );
        OwnershipEffect::Assigned {
            home: home.clone(),
            epoch,
        }
    }

    fn advance_epoch(&mut self, authority: &AuthorityKey, now_unix: i64) -> OwnershipEffect {
        let Some(record) = self.authorities.get_mut(&authority.0) else {
            return OwnershipEffect::Rejected(Rejection::NotAssigned);
        };
        expire_writes(record, now_unix);
        if !record.writes.is_empty() {
            return OwnershipEffect::Rejected(Rejection::WritesInFlight);
        }
        let epoch = AuthorityEpoch(record.epoch.0 + 1);
        record.epoch = epoch;
        OwnershipEffect::EpochAdvanced { epoch }
    }

    fn begin_write(
        &mut self,
        authority: &AuthorityKey,
        epoch: AuthorityEpoch,
        id: &str,
        issued_at_unix: i64,
        expires_at_unix: i64,
    ) -> OwnershipEffect {
        let Some(record) = self.authorities.get_mut(&authority.0) else {
            return OwnershipEffect::Rejected(Rejection::NotAssigned);
        };
        expire_writes(record, issued_at_unix);
        if record.epoch != epoch {
            return OwnershipEffect::Rejected(Rejection::EpochMismatch);
        }
        if expires_at_unix <= issued_at_unix
            || expires_at_unix.saturating_sub(issued_at_unix) > AUTHORITY_WRITE_LEASE_SECS
        {
            return OwnershipEffect::Rejected(Rejection::InvalidLease);
        }
        record
            .writes
            .insert(id.to_owned(), WriteLeaseRecord { epoch, expires_at_unix });
        OwnershipEffect::WriteLeased {
            epoch,
            id: id.to_owned(),
            expires_at_unix,
        }
    }

    fn finish_write(&mut self, authority: &AuthorityKey, epoch: AuthorityEpoch, id: &str) -> OwnershipEffect {
        let Some(record) = self.authorities.get_mut(&authority.0) else {
            return OwnershipEffect::Rejected(Rejection::NotAssigned);
        };
        if record.writes.get(id).is_some_and(|lease| lease.epoch != epoch) {
            return OwnershipEffect::Rejected(Rejection::EpochMismatch);
        }
        record.writes.remove(id);
        OwnershipEffect::WriteFinished
    }

    fn transfer(&mut self, authority: &AuthorityKey, new_home: &DatacenterId, now_unix: i64) -> OwnershipEffect {
        let Some(record) = self.authorities.get_mut(&authority.0) else {
            return OwnershipEffect::Rejected(Rejection::NotAssigned);
        };
        if record.home == *new_home {
            return OwnershipEffect::Rejected(Rejection::SameHome);
        }
        expire_writes(record, now_unix);
        if !record.writes.is_empty() {
            return OwnershipEffect::Rejected(Rejection::WritesInFlight);
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

fn expire_writes(record: &mut AuthorityRecord, now_unix: i64) -> usize {
    let before = record.writes.len();
    record
        .writes
        .retain(|_, lease| lease.expires_at_unix.saturating_add(AUTHORITY_CLOCK_SKEW_SECS) > now_unix);
    before - record.writes.len()
}
