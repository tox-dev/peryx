use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use peryx_storage::meta::{
    FinishJobRun, JobOutcome, JobRunRecord, JobRunStoreError, JobState, MetaError, MetaStore, NewJobRun,
};
use tokio_util::sync::CancellationToken;

/// The result of asking this node to cancel a durable attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelJobRun {
    Requested,
    Finished,
    Unavailable,
    Missing,
}

/// A durable attempt could not complete its lifecycle transition.
#[derive(Debug, thiserror::Error)]
pub enum JobAttemptError {
    #[error(transparent)]
    Start(#[from] JobRunStoreError),
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("durable job attempt disappeared before completion")]
    Missing,
    #[error("durable job attempt finished outside its active runner")]
    AlreadyFinished,
}

/// Durable attempt state shared by the scheduler and the management boundary.
pub struct JobAttemptControl {
    store: MetaStore,
    active: Mutex<HashMap<String, CancellationToken>>,
}

impl JobAttemptControl {
    #[must_use]
    pub fn new(store: MetaStore) -> Self {
        Self {
            store,
            active: Mutex::new(HashMap::new()),
        }
    }

    pub(super) fn start(&self, run: NewJobRun<'_>, cancel: CancellationToken) -> Result<String, JobAttemptError> {
        let mut active = self.lock();
        let id = self.store.start_job_run(run)?;
        active.insert(id.clone(), cancel);
        drop(active);
        Ok(id)
    }

    pub(super) fn finish(&self, id: &str, outcome: JobOutcome<'_>) -> Result<JobRunRecord, JobAttemptError> {
        let mut active = self.lock();
        let result = match self.store.finish_job_run(id, outcome)? {
            FinishJobRun::Finished(record) => {
                active.remove(id);
                Ok(record)
            }
            FinishJobRun::AlreadyFinished(_) => {
                active.remove(id);
                Err(JobAttemptError::AlreadyFinished)
            }
            FinishJobRun::Missing => Err(JobAttemptError::Missing),
        };
        drop(active);
        result
    }

    /// Signal one attempt owned by this process, or classify its durable state.
    ///
    /// # Errors
    /// Returns a store error if an inactive ID cannot be inspected.
    pub fn cancel(&self, id: &str) -> Result<CancelJobRun, MetaError> {
        let active = self.lock();
        if let Some(cancel) = active.get(id) {
            cancel.cancel();
            return Ok(CancelJobRun::Requested);
        }
        drop(active);
        Ok(match self.store.get_job_run(id)? {
            Some(record) if record.state == JobState::Running => CancelJobRun::Unavailable,
            Some(_) => CancelJobRun::Finished,
            None => CancelJobRun::Missing,
        })
    }

    /// Mark attempts left running by an earlier process as failed.
    ///
    /// # Errors
    /// Returns a store error if recovery cannot read or update durable history.
    pub fn recover_interrupted(&self, recovered_at_unix: i64) -> Result<usize, MetaError> {
        self.store.recover_interrupted_job_runs(recovered_at_unix)
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, CancellationToken>> {
        self.active.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
#[path = "../../tests/unit/jobs/attempts/tests.rs"]
mod tests;
