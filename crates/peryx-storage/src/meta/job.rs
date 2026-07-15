use redb::{ReadableDatabase as _, ReadableTable as _};
use serde::{Deserialize, Serialize};

use super::error::MetaError;
use super::{JOB_RUN, JOB_SERIAL_KEY, MetaStore, SERIAL};

/// The maintenance task a job run carried out. Each variant is produced by one background task; the
/// enum grows as scheduled kinds (mirror sync, cleanup, verify, backup) gain their runners.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// The background sweep that revalidates stale cached pages.
    CacheRefresh,
}

/// Where a job run stands: in flight, or finished one way or the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Running,
    Succeeded,
    Failed,
}

/// A durable record of one background job run: what ran, when, and how it ended. Written when a task
/// starts and updated when it finishes, so the read-only history survives a restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRunRecord {
    pub id: String,
    pub kind: JobKind,
    /// The repository or index the job acted on, or empty for a store-wide task.
    pub scope: String,
    pub state: JobState,
    pub started_at_unix: i64,
    pub finished_at_unix: Option<i64>,
    pub items_processed: u64,
    pub items_changed: u64,
    pub error: Option<String>,
}

/// The fields a caller supplies to open a job run.
#[derive(Debug, Clone, Copy)]
pub struct NewJobRun<'a> {
    pub kind: JobKind,
    pub scope: &'a str,
    pub started_at_unix: i64,
}

/// The result a caller records when a job run finishes.
#[derive(Debug, Clone, Copy)]
pub struct JobOutcome<'a> {
    pub state: JobState,
    pub finished_at_unix: i64,
    pub items_processed: u64,
    pub items_changed: u64,
    pub error: Option<&'a str>,
}

impl MetaStore {
    /// Open a job run in the `Running` state and return its ID.
    ///
    /// # Errors
    /// Returns a store error if the write fails or the record cannot be encoded.
    pub fn start_job_run(&self, run: NewJobRun<'_>) -> Result<String, MetaError> {
        let txn = self.db.begin_write()?;
        let id = {
            let mut serials = txn.open_table(SERIAL)?;
            let next = serials.get(JOB_SERIAL_KEY)?.map_or(0, |value| value.value()) + 1;
            serials.insert(JOB_SERIAL_KEY, next)?;
            format!("jr_{next:016x}")
        };
        let record = JobRunRecord {
            id: id.clone(),
            kind: run.kind,
            scope: run.scope.to_owned(),
            state: JobState::Running,
            started_at_unix: run.started_at_unix,
            finished_at_unix: None,
            items_processed: 0,
            items_changed: 0,
            error: None,
        };
        {
            let bytes = serde_json::to_vec(&record)?;
            txn.open_table(JOB_RUN)?.insert(id.as_str(), bytes.as_slice())?;
        }
        txn.commit()?;
        Ok(id)
    }

    /// Record the outcome of a job run, returning the updated record when it still exists.
    ///
    /// # Errors
    /// Returns a store error if the write fails or the record cannot be decoded or encoded.
    pub fn finish_job_run(&self, id: &str, outcome: JobOutcome<'_>) -> Result<Option<JobRunRecord>, MetaError> {
        let txn = self.db.begin_write()?;
        let Some(mut record) = ({
            let table = txn.open_table(JOB_RUN)?;
            table
                .get(id)?
                .map(|value| serde_json::from_slice::<JobRunRecord>(value.value()))
                .transpose()?
        }) else {
            return Ok(None);
        };
        record.state = outcome.state;
        record.finished_at_unix = Some(outcome.finished_at_unix);
        record.items_processed = outcome.items_processed;
        record.items_changed = outcome.items_changed;
        record.error = outcome.error.map(str::to_owned);
        {
            let bytes = serde_json::to_vec(&record)?;
            txn.open_table(JOB_RUN)?.insert(id, bytes.as_slice())?;
        }
        txn.commit()?;
        Ok(Some(record))
    }

    /// Fetch one job run by ID.
    ///
    /// # Errors
    /// Returns a store error if the read fails or the record cannot be decoded.
    pub fn get_job_run(&self, id: &str) -> Result<Option<JobRunRecord>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(JOB_RUN)?;
        Ok(table
            .get(id)?
            .map(|value| serde_json::from_slice(value.value()))
            .transpose()?)
    }

    /// Job runs newest first. The ID encodes a monotonic serial, so reverse key order is reverse
    /// chronological without a second index.
    ///
    /// # Errors
    /// Returns a store error if the read fails or a record cannot be decoded.
    pub fn list_job_runs(&self) -> Result<Vec<JobRunRecord>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(JOB_RUN)?;
        let mut runs = Vec::new();
        for entry in table.iter()?.rev() {
            let (_, value) = entry?;
            runs.push(serde_json::from_slice(value.value())?);
        }
        Ok(runs)
    }
}
