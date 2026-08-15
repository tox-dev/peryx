//! Epoch-fenced leases prevent superseded workers from running cluster jobs. Callers supply epochs and
//! timestamps; storage does not choose leaders or read a clock.

use redb::ReadableTable as _;
use serde::{Deserialize, Serialize};

use super::{JOB_LEASE, MetaError, MetaStore};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum LeaseState {
    Held,
    /// A claim at or above the recorded epoch may acquire the job.
    Released,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobLease {
    pub job: String,
    /// Opaque worker identity.
    pub holder: String,
    /// Lower epochs cannot change the lease.
    pub epoch: u64,
    pub state: LeaseState,
    pub claimed_at_unix: i64,
    pub renewed_at_unix: i64,
    /// Used for scheduler liveness only; fencing ignores expiry.
    pub expires_at_unix: i64,
}

impl JobLease {
    /// Expiry does not release ownership; a later claim must supersede the lease.
    #[must_use]
    pub const fn is_expired(&self, now: i64) -> bool {
        matches!(self.state, LeaseState::Held) && now >= self.expires_at_unix
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// A first claim, released lease, newer epoch, or different holder acquired the lease.
    Granted(JobLease),
    /// The current holder extended its deadline at the same epoch.
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
    /// Grants claims at or above the stored epoch. The current holder at the same epoch renews its
    /// deadline without changing the original claim time; lower epochs cannot change the lease.
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

    /// Only the current holder at or above the stored epoch may release. Missing and released leases
    /// return `false`; a release records its epoch to preserve the fence.
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

    /// Returns leases in job-identity order.
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
