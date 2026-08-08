//! Durable, epoch-fenced ownership of a cluster background job.
//!
//! The job-run ledger in [`job`](super::job) records what a background task did; it says nothing about
//! who is allowed to run it. In a cluster a job like a reclamation sweep or a search rebuild must run
//! on one node at a time, and a node that a newer authority term has superseded must stop before its
//! writes collide with its successor's. This module records one durable lease per job identity and
//! enforces that with a fencing epoch: a claim at an epoch at or above the recorded one wins and names
//! its holder, and a claim from a superseded epoch is rejected without touching the lease.
//!
//! The epoch is supplied by the caller. The cluster authority that mints it - the ownership state
//! machine and its leadership term - lives above this layer and is still being built, so this module
//! stays a pure decision core over a `u64`, indifferent to whether that authority ends up Raft-based
//! or bespoke. It reads no clock and opens no socket: every time bound arrives as a parameter, so a
//! test drives the whole lifecycle deterministically. The scheduler that decides which jobs to claim
//! and how often to renew composes above.

use redb::ReadableTable as _;
use serde::{Deserialize, Serialize};

use super::{JOB_LEASE, MetaError, MetaStore};

/// Whether a lease is currently held or was given up by its holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum LeaseState {
    /// A holder owns the job under [`epoch`](JobLease::epoch).
    Held,
    /// The last holder released the job; a claim at or above the recorded epoch may take it.
    Released,
}

/// A durable lease naming the current owner of one background job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobLease {
    pub job: String,
    /// The worker identity that holds, or last held, the job. Opaque to the store.
    pub holder: String,
    /// The authority epoch the lease was written under; a lower epoch is a superseded worker and is
    /// fenced out without changing the lease.
    pub epoch: u64,
    pub state: LeaseState,
    pub claimed_at_unix: i64,
    pub renewed_at_unix: i64,
    /// When the lease lapses absent a renewal, so a scheduler can flag a holder that stopped renewing.
    /// The fencing decision never consults it; a superseding epoch reclaims a lease whether or not it
    /// has lapsed.
    pub expires_at_unix: i64,
}

impl JobLease {
    /// Whether the lease has lapsed as of `now`, the signal a scheduler uses to notice a holder that
    /// stopped renewing. A lapsed lease is still owned until a claim supersedes it.
    #[must_use]
    pub const fn is_expired(&self, now: i64) -> bool {
        matches!(self.state, LeaseState::Held) && now >= self.expires_at_unix
    }
}

/// The effect of claiming a job lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// The lease was acquired: a first claim, a takeover of a released lease, or a supersede by a newer
    /// epoch or a different holder.
    Granted(JobLease),
    /// The current holder re-claimed its own lease at its own epoch and extended the deadline.
    Renewed(JobLease),
}

impl ClaimOutcome {
    #[must_use]
    pub const fn lease(&self) -> &JobLease {
        match self {
            Self::Granted(lease) | Self::Renewed(lease) => lease,
        }
    }
}

/// A rejected lease operation.
#[derive(Debug, thiserror::Error)]
pub enum JobLeaseError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("a newer fence {current} supersedes the applied fence {applied}")]
    StaleFence { current: u64, applied: u64 },
    #[error("the job is held by {holder:?}, not the caller")]
    NotHolder { holder: String },
}

impl MetaStore {
    /// Claim the lease for `job` under a fencing epoch, acquiring or renewing it.
    ///
    /// An epoch at or above the recorded one wins: a first claim, a claim on a released lease, a claim
    /// by a newer epoch, or a claim by a different holder at the same epoch all acquire the lease and
    /// return [`Granted`](ClaimOutcome::Granted), reclaiming it from a superseded holder in the process.
    /// The current holder re-claiming at its own epoch returns [`Renewed`](ClaimOutcome::Renewed) with a
    /// deadline extended to `now + lease_secs` and its original claim time preserved. A claim from an
    /// epoch below the recorded one is a superseded worker and is rejected without change.
    ///
    /// # Errors
    /// Returns [`JobLeaseError::StaleFence`] when `epoch` is below the recorded one, or a store error
    /// when the row cannot be read, encoded, or committed.
    pub fn claim_job_lease(
        &self,
        job: &str,
        holder: &str,
        epoch: u64,
        now: i64,
        lease_secs: i64,
    ) -> Result<ClaimOutcome, JobLeaseError> {
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        let existing = read_lease(&txn, job)?;
        guard_fence(existing.as_ref(), epoch)?;
        let (renewed, claimed_at_unix) = match &existing {
            Some(lease)
                if matches!(lease.state, LeaseState::Held) && lease.holder == holder && lease.epoch == epoch =>
            {
                (true, lease.claimed_at_unix)
            }
            _ => (false, now),
        };
        let lease = JobLease {
            job: job.to_owned(),
            holder: holder.to_owned(),
            epoch,
            state: LeaseState::Held,
            claimed_at_unix,
            renewed_at_unix: now,
            expires_at_unix: now.saturating_add(lease_secs),
        };
        write_lease(&txn, &lease)?;
        txn.commit().map_err(MetaError::from)?;
        Ok(if renewed {
            ClaimOutcome::Renewed(lease)
        } else {
            ClaimOutcome::Granted(lease)
        })
    }

    /// Release the lease for `job`, so the next claim at or above the current epoch may take it.
    ///
    /// Only the current holder may release, and only from an epoch at or above the recorded one. A
    /// release of a missing or already-released lease is a no-op that reports `false`. Releasing marks
    /// the lease [`Released`](LeaseState::Released) and stamps the releasing epoch, so a later stale
    /// claim still loses the fence.
    ///
    /// # Errors
    /// Returns [`JobLeaseError::StaleFence`] when `epoch` is below the recorded one,
    /// [`JobLeaseError::NotHolder`] when a different holder owns the lease, or a store error when the row
    /// cannot be read, encoded, or committed.
    pub fn release_job_lease(&self, job: &str, holder: &str, epoch: u64) -> Result<bool, JobLeaseError> {
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        let existing = read_lease(&txn, job)?;
        guard_fence(existing.as_ref(), epoch)?;
        let released = match existing {
            None => false,
            Some(lease) if matches!(lease.state, LeaseState::Released) => false,
            Some(lease) if lease.holder != holder => {
                return Err(JobLeaseError::NotHolder { holder: lease.holder });
            }
            Some(lease) => {
                let freed = JobLease {
                    epoch,
                    state: LeaseState::Released,
                    ..lease
                };
                write_lease(&txn, &freed)?;
                true
            }
        };
        txn.commit().map_err(MetaError::from)?;
        Ok(released)
    }

    /// Read the lease for one job.
    ///
    /// # Errors
    /// Returns a store error when the row cannot be read or decoded.
    pub fn job_lease(&self, job: &str) -> Result<Option<JobLease>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(JOB_LEASE)?;
        Ok(table
            .get(job)?
            .map(|value| serde_json::from_slice(value.value()))
            .transpose()?)
    }

    /// List every job lease in job-identity order.
    ///
    /// # Errors
    /// Returns a store error when a row cannot be read or decoded.
    pub fn job_leases(&self) -> Result<Vec<JobLease>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(JOB_LEASE)?;
        let mut leases = Vec::new();
        for entry in table.iter()? {
            let (_key, value) = entry?;
            leases.push(serde_json::from_slice(value.value())?);
        }
        Ok(leases)
    }
}

const fn guard_fence(existing: Option<&JobLease>, epoch: u64) -> Result<(), JobLeaseError> {
    if let Some(lease) = existing
        && epoch < lease.epoch
    {
        return Err(JobLeaseError::StaleFence {
            current: lease.epoch,
            applied: epoch,
        });
    }
    Ok(())
}

fn read_lease(txn: &redb::WriteTransaction, job: &str) -> Result<Option<JobLease>, MetaError> {
    let table = txn.open_table(JOB_LEASE)?;
    Ok(table
        .get(job)?
        .map(|value| serde_json::from_slice(value.value()))
        .transpose()?)
}

fn write_lease(txn: &redb::WriteTransaction, lease: &JobLease) -> Result<(), MetaError> {
    let value = serde_json::to_vec(lease)?;
    txn.open_table(JOB_LEASE)?
        .insert(lease.job.as_str(), value.as_slice())?;
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/meta/job_lease_fault_tests.rs"]
mod fault_tests;
