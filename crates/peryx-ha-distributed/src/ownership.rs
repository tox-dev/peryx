//! Epoch zero means unassigned. Assignment starts at one; advances and transfers increase the epoch.
//! Invalid transitions leave the state unchanged, so all replicas derive the same state from the same
//! command sequence.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::authority::AuthorityKey;
use crate::envelope::AuthorityEpoch;
use peryx_ha::{
    AUTHORITY_CLOCK_SKEW_SECS, AUTHORITY_WRITE_LEASE_SECS, CONTROL_IDEMPOTENCY_SECS, CommandOutcome, CommandReceipt,
    ControlCommand, PendingTransferAudit, SINGLETON_LEASE_SECS, TransferAudit,
};

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
    /// Moves an assigned authority and increases its epoch.
    RecordTransfer {
        authority: AuthorityKey,
        new_home: DatacenterId,
        now_unix: i64,
    },
    /// Drops an authority the deployment no longer serves, so its record leaves the replicated state.
    ForgetAuthority { authority: AuthorityKey, now_unix: i64 },
    /// Grants a cluster-singleton job to one holder at the next generation.
    AcquireSingletonLease {
        job: String,
        holder: String,
        now_unix: i64,
        expires_at_unix: i64,
    },
    /// Extends a grant the presented holder, term, and generation still own.
    RenewSingletonLease {
        job: String,
        holder: String,
        term: u64,
        generation: u64,
        now_unix: i64,
        expires_at_unix: i64,
    },
    /// Frees a grant the presented holder, term, and generation still own.
    ReleaseSingletonLease {
        job: String,
        holder: String,
        term: u64,
        generation: u64,
        now_unix: i64,
    },
    /// Binds `key` to `command` and, when consensus itself carries the mutation, applies it and records
    /// its receipt in the same decision.
    AttemptControl {
        key: String,
        command: ControlCommand,
        now_unix: i64,
    },
    /// Records the receipt of a control command whose mutation is a consensus membership change, which
    /// no ownership decision can carry.
    SettleControl {
        key: String,
        command: ControlCommand,
        receipt: CommandReceipt,
        now_unix: i64,
    },
    /// Frees the claim of a membership change that failed, so the key answers a retry immediately
    /// instead of standing until it ages out.
    ReleaseControl {
        key: String,
        command: ControlCommand,
        now_unix: i64,
    },
    /// Marks one member's durable store as holding the audit sealed under `key`.
    CompleteTransferAudit { key: String, projector: String },
}

/// What the replicated idempotency window says about one keyed control attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlResolution {
    /// The mutation applied in this decision and its receipt is now durable.
    Committed(CommandReceipt),
    /// An earlier attempt under this key already recorded a receipt.
    Replayed(CommandReceipt),
    /// The key is bound; the caller owes the membership change and its settlement.
    Claimed,
    /// The mutation left ownership unchanged, so nothing was recorded.
    Rejected(ControlRejection),
    /// The key already stands for a different command.
    KeyReuse,
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
    /// The authority left the replicated state at the epoch it last held.
    Forgotten {
        epoch: AuthorityEpoch,
    },
    /// Nothing was homed under the authority, which is the end state the command asked for.
    AlreadyForgotten,
    SingletonAcquired {
        holder: String,
        term: u64,
        generation: u64,
        expires_at_unix: i64,
    },
    /// The claim lost to an unlapsed grant, which the named holder keeps.
    SingletonHeld {
        holder: String,
    },
    SingletonRenewed {
        expires_at_unix: i64,
    },
    SingletonReleased,
    Control(ControlResolution),
    /// The receipt that stands for the key, which is the first one recorded under it.
    ControlSettled(CommandReceipt),
    /// The key no longer holds an unfinished claim, whether this decision freed one or a retry had
    /// already settled it.
    ControlReleased,
    /// The key holds no unprojected audit, whether this decision dropped one or an earlier pass had.
    TransferAuditCompleted,
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
    /// The presented holder, term, and generation no longer own the singleton grant.
    SingletonLost,
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
    #[serde(default)]
    writes: BTreeMap<String, WriteLeaseRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WriteLeaseRecord {
    epoch: AuthorityEpoch,
    expires_at_unix: i64,
}

/// Kept after a release so a later grant of the same job never repeats a generation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct SingletonRecord {
    generation: u64,
    held: Option<SingletonHold>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SingletonHold {
    holder: String,
    term: u64,
    expires_at_unix: i64,
}

impl SingletonHold {
    /// A grant stays exclusive a clock-skew margin past its deadline, so the outgoing holder's view of
    /// the deadline can never overlap the next holder's grant.
    const fn lapsed(&self, now_unix: i64) -> bool {
        now_unix >= self.expires_at_unix.saturating_add(AUTHORITY_CLOCK_SKEW_SECS)
    }

    fn owned_by(&self, holder: &str, term: u64) -> bool {
        self.term == term && self.holder == holder
    }
}

/// One idempotency key's binding. A record without a receipt is a claim whose membership change is
/// still running; a retry of the same command re-runs it, which membership changes converge on. An
/// attempt that fails releases its record instead of leaving it to age out, so a failed key is open to
/// any command a retry carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ControlRecord {
    command: ControlCommand,
    /// The window is anchored at the first claim, so settlement never extends it.
    claimed_at_unix: i64,
    receipt: Option<CommandReceipt>,
    /// Set while at least one voter present at commit has not projected this audit. Snapshots retain
    /// the audit until those voters project it or leave the membership.
    #[serde(default)]
    audit: Option<TransferAudit>,
    #[serde(default)]
    audit_projectors: BTreeSet<String>,
}

/// Missing authorities are unassigned and read as epoch zero; missing singletons are unheld.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnershipState {
    authorities: BTreeMap<String, AuthorityRecord>,
    singletons: BTreeMap<String, SingletonRecord>,
    controls: BTreeMap<String, ControlRecord>,
    #[serde(default)]
    audit_projectors: BTreeSet<String>,
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
            OwnershipCommand::ForgetAuthority { authority, now_unix } => self.forget(authority, *now_unix),
            OwnershipCommand::AcquireSingletonLease {
                job,
                holder,
                now_unix,
                expires_at_unix,
            } => self.acquire_singleton(job, holder, *now_unix, *expires_at_unix, meta),
            OwnershipCommand::RenewSingletonLease {
                job,
                holder,
                term,
                generation,
                now_unix,
                expires_at_unix,
            } => self.renew_singleton(job, holder, *term, *generation, *now_unix, *expires_at_unix),
            OwnershipCommand::ReleaseSingletonLease {
                job,
                holder,
                term,
                generation,
                now_unix,
            } => self.release_singleton(job, holder, *term, *generation, *now_unix),
            OwnershipCommand::AttemptControl { key, command, now_unix } => {
                OwnershipEffect::Control(self.attempt_control(key, command, *now_unix, meta))
            }
            OwnershipCommand::SettleControl {
                key,
                command,
                receipt,
                now_unix,
            } => OwnershipEffect::ControlSettled(self.settle_control(key, command, receipt, *now_unix)),
            OwnershipCommand::ReleaseControl { key, command, now_unix } => {
                self.release_control(key, command, *now_unix);
                OwnershipEffect::ControlReleased
            }
            OwnershipCommand::CompleteTransferAudit { key, projector } => {
                self.complete_transfer_audit(key, projector);
                OwnershipEffect::TransferAuditCompleted
            }
        }
    }

    /// Resolves `key` against the idempotency window and, for the commands ownership itself carries,
    /// applies the mutation and records its receipt in this same decision. Fusing the two is what stops
    /// a replacement leader from committing a second mutation for a request it never answered.
    fn attempt_control(
        &mut self,
        key: &str,
        command: &ControlCommand,
        now_unix: i64,
        meta: AppliedMeta,
    ) -> ControlResolution {
        self.prune_controls(now_unix);
        if let Some(record) = self.controls.get(key) {
            if record.command != *command {
                return ControlResolution::KeyReuse;
            }
            let Some(receipt) = record.receipt.clone() else {
                return ControlResolution::Claimed;
            };
            return ControlResolution::Replayed(receipt);
        }
        let Some(effect) = self.apply_control(command, now_unix) else {
            self.bind_control(key, command, now_unix, None, None, BTreeSet::new());
            return ControlResolution::Claimed;
        };
        let outcome = match control_outcome(&effect) {
            Ok(outcome) => outcome,
            Err(rejection) => return ControlResolution::Rejected(rejection),
        };
        let audit = self.seal_transfer_audit(command, meta);
        let audit_projectors = audit
            .as_ref()
            .map_or_else(BTreeSet::new, |_| self.audit_projectors.clone());
        let receipt = CommandReceipt {
            term: meta.term,
            index: meta.index,
            outcome,
            old_voters: Vec::new(),
            new_voters: Vec::new(),
            transfer_audit: audit.clone().map(Box::new),
        };
        self.bind_control(key, command, now_unix, Some(receipt.clone()), audit, audit_projectors);
        ControlResolution::Committed(receipt)
    }

    /// Seals the post-mutation epoch and the deciding log position into a transfer audit.
    fn seal_transfer_audit(&self, command: &ControlCommand, meta: AppliedMeta) -> Option<TransferAudit> {
        let ControlCommand::TransferAuthority {
            authority,
            new_home,
            intent,
        } = command
        else {
            return None;
        };
        let intent = intent.as_ref()?;
        Some(TransferAudit {
            authority: authority.clone(),
            source: intent.source.clone(),
            target: new_home.clone(),
            actor: intent.actor.clone(),
            reason: intent.reason.clone(),
            barrier: intent.barrier,
            epoch: self.epoch(&AuthorityKey(authority.clone())).0,
            commit_term: meta.term,
            commit_index: meta.index,
        })
    }

    /// Audits sealed by a committed transfer that `projector` has not reported storing.
    #[must_use]
    pub(crate) fn pending_transfer_audits(&self, projector: &str) -> Vec<PendingTransferAudit> {
        self.controls
            .iter()
            .filter_map(|(key, record)| {
                let audit = record.audit.as_ref()?;
                record
                    .audit_projectors
                    .contains(projector)
                    .then(|| PendingTransferAudit {
                        id: key.clone(),
                        audit: audit.clone(),
                    })
            })
            .collect()
    }

    fn complete_transfer_audit(&mut self, key: &str, projector: &str) {
        if let Some(record) = self.controls.get_mut(key) {
            record.audit_projectors.remove(projector);
            if record.audit_projectors.is_empty() {
                record.audit = None;
            }
        }
    }

    pub(crate) fn set_audit_projectors(&mut self, projectors: BTreeSet<String>) {
        for record in self.controls.values_mut() {
            if let (Some(audit), Some(receipt)) = (&record.audit, &mut record.receipt)
                && receipt.transfer_audit.is_none()
            {
                receipt.transfer_audit = Some(Box::new(audit.clone()));
            }
            if !projectors.is_empty() {
                if record.audit.is_some() && record.audit_projectors.is_empty() {
                    // Older snapshots carried the audit without its voter roster or receipt copy. Bind
                    // those records to the membership stored with the snapshot.
                    record.audit_projectors.clone_from(&projectors);
                } else {
                    record
                        .audit_projectors
                        .retain(|projector| projectors.contains(projector));
                    if record.audit_projectors.is_empty() {
                        record.audit = None;
                    }
                }
            }
        }
        self.audit_projectors = projectors;
    }

    /// The ownership mutation `command` performs, or `None` when a consensus membership change carries
    /// it instead.
    fn apply_control(&mut self, command: &ControlCommand, now_unix: i64) -> Option<OwnershipEffect> {
        match command {
            ControlCommand::TransferAuthority {
                authority, new_home, ..
            } => Some(self.transfer(
                &AuthorityKey(authority.clone()),
                &DatacenterId(new_home.clone()),
                now_unix,
            )),
            ControlCommand::AdvanceEpoch { authority } => {
                Some(self.advance_epoch(&AuthorityKey(authority.clone()), now_unix))
            }
            ControlCommand::ForgetAuthority { authority } => {
                Some(self.forget(&AuthorityKey(authority.clone()), now_unix))
            }
            ControlCommand::AddLearner { .. }
            | ControlCommand::PromoteVoter { .. }
            | ControlCommand::RemoveVoter { .. }
            | ControlCommand::ReplaceVoter { .. } => None,
        }
    }

    /// Keeps the receipt of the first attempt, so a settlement that arrives after a retry already
    /// committed one cannot overwrite the answer the caller was given.
    fn settle_control(
        &mut self,
        key: &str,
        command: &ControlCommand,
        receipt: &CommandReceipt,
        now_unix: i64,
    ) -> CommandReceipt {
        self.prune_controls(now_unix);
        self.controls
            .entry(key.to_owned())
            .or_insert_with(|| ControlRecord {
                command: command.clone(),
                claimed_at_unix: now_unix,
                receipt: None,
                audit: None,
                audit_projectors: BTreeSet::new(),
            })
            .receipt
            .get_or_insert_with(|| receipt.clone())
            .clone()
    }

    /// Drops the claim `command` failed to complete. A record carrying a receipt belongs to an attempt
    /// that succeeded, and one bound to another command belongs to a key that was rebound after the
    /// prune, so neither is freed by a release that arrives late.
    fn release_control(&mut self, key: &str, command: &ControlCommand, now_unix: i64) {
        self.prune_controls(now_unix);
        if self
            .controls
            .get(key)
            .is_some_and(|record| record.receipt.is_none() && record.command == *command)
        {
            self.controls.remove(key);
        }
    }

    fn bind_control(
        &mut self,
        key: &str,
        command: &ControlCommand,
        now_unix: i64,
        receipt: Option<CommandReceipt>,
        audit: Option<TransferAudit>,
        audit_projectors: BTreeSet<String>,
    ) {
        self.controls.insert(
            key.to_owned(),
            ControlRecord {
                command: command.clone(),
                claimed_at_unix: now_unix,
                receipt,
                audit,
                audit_projectors,
            },
        );
    }

    /// Every replica prunes from the `now_unix` of the same committed entry, so the window stays
    /// identical across the group. A record still holding an unprojected audit outlives the window: the
    /// move it describes is committed, so dropping it would lose the only durable trace of that move.
    fn prune_controls(&mut self, now_unix: i64) {
        self.controls.retain(|_, record| {
            record.audit.is_some() || now_unix.saturating_sub(record.claimed_at_unix) < CONTROL_IDEMPOTENCY_SECS
        });
    }

    /// Grants `job` to `holder` when no unlapsed grant stands, at a generation above every earlier grant
    /// of the same job. The term comes from the committed entry, so a claim cannot name its own.
    fn acquire_singleton(
        &mut self,
        job: &str,
        holder: &str,
        now_unix: i64,
        expires_at_unix: i64,
        meta: AppliedMeta,
    ) -> OwnershipEffect {
        if !bounded_singleton_lease(now_unix, expires_at_unix) {
            return OwnershipEffect::Rejected(Rejection::InvalidLease);
        }
        let record = self.singletons.entry(job.to_owned()).or_default();
        if let Some(held) = &record.held
            && !held.lapsed(now_unix)
        {
            return OwnershipEffect::SingletonHeld {
                holder: held.holder.clone(),
            };
        }
        record.generation += 1;
        record.held = Some(SingletonHold {
            holder: holder.to_owned(),
            term: meta.term,
            expires_at_unix,
        });
        OwnershipEffect::SingletonAcquired {
            holder: holder.to_owned(),
            term: meta.term,
            generation: record.generation,
            expires_at_unix,
        }
    }

    /// A renewal that arrives after the grant lapsed is refused rather than applied, so a delayed
    /// keepalive can never revive ownership the authority has already let go.
    fn renew_singleton(
        &mut self,
        job: &str,
        holder: &str,
        term: u64,
        generation: u64,
        now_unix: i64,
        expires_at_unix: i64,
    ) -> OwnershipEffect {
        if !bounded_singleton_lease(now_unix, expires_at_unix) {
            return OwnershipEffect::Rejected(Rejection::InvalidLease);
        }
        let Some(record) = self.owned_singleton(job, holder, term, generation, now_unix) else {
            return OwnershipEffect::Rejected(Rejection::SingletonLost);
        };
        record.held = Some(SingletonHold {
            holder: holder.to_owned(),
            term,
            expires_at_unix,
        });
        OwnershipEffect::SingletonRenewed { expires_at_unix }
    }

    /// Frees the grant but keeps its generation, so the next holder of the same job under the same term
    /// still gets a higher one and the released holder's late request stays fenced.
    fn release_singleton(
        &mut self,
        job: &str,
        holder: &str,
        term: u64,
        generation: u64,
        now_unix: i64,
    ) -> OwnershipEffect {
        let Some(record) = self.owned_singleton(job, holder, term, generation, now_unix) else {
            return OwnershipEffect::Rejected(Rejection::SingletonLost);
        };
        record.held = None;
        OwnershipEffect::SingletonReleased
    }

    /// The record for `job` when the presented identity still owns an unlapsed grant of it.
    fn owned_singleton(
        &mut self,
        job: &str,
        holder: &str,
        term: u64,
        generation: u64,
        now_unix: i64,
    ) -> Option<&mut SingletonRecord> {
        let record = self.singletons.get_mut(job)?;
        let owned = record.generation == generation
            && record
                .held
                .as_ref()
                .is_some_and(|held| held.owned_by(holder, term) && !held.lapsed(now_unix));
        owned.then_some(record)
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
        OwnershipEffect::Transferred {
            from,
            to: new_home.clone(),
            epoch,
        }
    }

    /// Removes the authority so its home, epoch, and assignment stop travelling in every snapshot.
    /// A live write lease blocks the removal, because the lease holder still stamps work with the epoch
    /// this drops.
    fn forget(&mut self, authority: &AuthorityKey, now_unix: i64) -> OwnershipEffect {
        let Some(record) = self.authorities.get_mut(&authority.0) else {
            return OwnershipEffect::AlreadyForgotten;
        };
        expire_writes(record, now_unix);
        if !record.writes.is_empty() {
            return OwnershipEffect::Rejected(Rejection::WritesInFlight);
        }
        let epoch = record.epoch;
        self.authorities.remove(&authority.0);
        OwnershipEffect::Forgotten { epoch }
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

    /// # Panics
    /// JSON serialization failure, which the state's field types make unreachable.
    #[must_use]
    pub fn snapshot(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("an ownership state always serializes to JSON")
    }

    /// Rejects homed authorities at epoch zero because the fence reserves zero for unassigned state.
    ///
    /// # Errors
    /// [`OwnershipError::Malformed`] when the bytes are not a valid snapshot, or
    /// [`OwnershipError::ZeroEpoch`] when a homed authority carries the reserved zero epoch.
    pub fn restore(bytes: &[u8]) -> Result<Self, OwnershipError> {
        let state: Self = serde_json::from_slice(bytes).map_err(OwnershipError::Malformed)?;
        for (authority, record) in &state.authorities {
            if record.epoch == UNASSIGNED {
                return Err(OwnershipError::ZeroEpoch {
                    authority: authority.clone(),
                });
            }
        }
        Ok(state)
    }
}

/// Why an authority command left ownership unchanged. A rejected command records no receipt, so its
/// idempotency key stays open to a retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlRejection {
    NotAssigned,
    WritesInFlight,
}

/// The outcome an authority-command effect commits.
///
/// Moving an authority to its current home, and forgetting one the state never held, are the requested
/// end states, so both commit as no-ops rather than failing.
///
/// # Errors
/// Returns the [`ControlRejection`] for an effect that left ownership unchanged and so recorded no
/// receipt.
pub const fn control_outcome(effect: &OwnershipEffect) -> Result<CommandOutcome, ControlRejection> {
    match effect {
        OwnershipEffect::Rejected(Rejection::SameHome) | OwnershipEffect::AlreadyForgotten => {
            Ok(CommandOutcome::NoChange)
        }
        OwnershipEffect::Rejected(Rejection::NotAssigned) => Err(ControlRejection::NotAssigned),
        OwnershipEffect::Rejected(Rejection::WritesInFlight) => Err(ControlRejection::WritesInFlight),
        _ => Ok(CommandOutcome::Committed),
    }
}

/// A grant must end after it starts and may not outlive [`SINGLETON_LEASE_SECS`], so no worker can
/// propose ownership longer than the authority's policy allows.
const fn bounded_singleton_lease(now_unix: i64, expires_at_unix: i64) -> bool {
    expires_at_unix > now_unix && expires_at_unix.saturating_sub(now_unix) <= SINGLETON_LEASE_SECS
}

fn expire_writes(record: &mut AuthorityRecord, now_unix: i64) -> usize {
    let before = record.writes.len();
    record
        .writes
        .retain(|_, lease| lease.expires_at_unix.saturating_add(AUTHORITY_CLOCK_SKEW_SECS) > now_unix);
    before - record.writes.len()
}
