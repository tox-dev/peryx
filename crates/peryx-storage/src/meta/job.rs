use std::ops::Bound::{Excluded, Unbounded};

use redb::{ReadableTable as _, ReadableTableMetadata as _};
use serde::{Deserialize, Serialize};

use super::error::MetaError;
use super::{JOB_RUN, JOB_SERIAL_KEY, MetaStore, SERIAL};

const JOB_RETENTION_BATCH: usize = 128;
const MAX_QUERY_LIMIT: usize = 100;
const MAX_ERROR_BYTES: usize = 2_048;
const MAX_SCOPE_BYTES: usize = 512;
const RESTART_ERROR: &str = "node restarted before the job finished";

/// The maintenance task a job run carried out. Each variant is produced by one background task; the
/// enum grows as scheduled kinds (mirror sync, cleanup, verify, backup) gain their runners.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// Finalizing a failed home's retained ingress intents at the new home after authority transfer.
    AuthorityDrain,
    /// The background sweep that revalidates stale cached pages.
    CacheRefresh,
    /// A bounded remote project-catalog and file-metadata refresh.
    CatalogSync,
    /// A full rebuild of the derived package search index from authoritative metadata.
    SearchRebuild,
}

/// Where a job run stands: in flight, or finished one way or the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

/// A durable record of one background job run: what ran, when, and how it ended. Written when a task
/// starts and updated when it finishes, so the read-only history survives a restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRunRecord {
    pub id: String,
    pub kind: JobKind,
    /// The repository or index the job acted on, or empty for a store-wide task.
    pub scope: String,
    /// The configured repository this run is authorized against. Node- and ecosystem-wide work has
    /// no repository owner and remains visible only through operator authorization.
    #[serde(default)]
    pub repository: Option<String>,
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
    pub repository: Option<&'a str>,
    pub started_at_unix: i64,
}

/// The result a caller records when a job run finishes.
#[derive(Debug, Clone, Copy)]
pub struct JobOutcome<'a> {
    state: JobState,
    finished_at_unix: i64,
    items_processed: u64,
    items_changed: u64,
    error: Option<&'a str>,
}

impl<'a> JobOutcome<'a> {
    #[must_use]
    pub const fn succeeded(finished_at_unix: i64, items_processed: u64, items_changed: u64) -> Self {
        Self {
            state: JobState::Succeeded,
            finished_at_unix,
            items_processed,
            items_changed,
            error: None,
        }
    }

    #[must_use]
    pub const fn failed(finished_at_unix: i64, items_processed: u64, items_changed: u64, error: &'a str) -> Self {
        Self {
            state: JobState::Failed,
            finished_at_unix,
            items_processed,
            items_changed,
            error: Some(error),
        }
    }

    #[must_use]
    pub const fn cancelled(finished_at_unix: i64, items_processed: u64, items_changed: u64) -> Self {
        Self {
            state: JobState::Cancelled,
            finished_at_unix,
            items_processed,
            items_changed,
            error: None,
        }
    }
}

/// The durable result of closing one running attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishJobRun {
    Finished(JobRunRecord),
    AlreadyFinished(JobRunRecord),
    Missing,
}

/// A bounded stable query over job-run history, newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRunQuery {
    pub cursor: Option<String>,
    pub limit: usize,
}

impl Default for JobRunQuery {
    fn default() -> Self {
        Self {
            cursor: None,
            limit: 25,
        }
    }
}

/// One page of job-run history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JobRunPage {
    #[serde(rename = "attempts")]
    pub runs: Vec<JobRunRecord>,
    pub next_cursor: Option<String>,
}

/// A rejected job-run write.
#[derive(Debug, thiserror::Error)]
pub enum JobRunStoreError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("scope exceeds {MAX_SCOPE_BYTES} bytes")]
    ScopeTooLong,
    #[error("repository exceeds {MAX_SCOPE_BYTES} bytes")]
    RepositoryTooLong,
}

/// A rejected job-run query.
#[derive(Debug, thiserror::Error)]
pub enum JobRunQueryError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("limit must be between 1 and {MAX_QUERY_LIMIT}")]
    InvalidLimit,
    #[error("invalid job run cursor")]
    InvalidCursor,
}

impl MetaStore {
    /// Open a job run in the `Running` state and return its ID.
    ///
    /// # Errors
    /// Returns a store error if the write fails or the record cannot be encoded.
    pub fn start_job_run(&self, run: NewJobRun<'_>) -> Result<String, JobRunStoreError> {
        if run.scope.len() > MAX_SCOPE_BYTES {
            return Err(JobRunStoreError::ScopeTooLong);
        }
        if run
            .repository
            .is_some_and(|repository| repository.len() > MAX_SCOPE_BYTES)
        {
            return Err(JobRunStoreError::RepositoryTooLong);
        }
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        let id = {
            let mut serials = txn.open_table(SERIAL).map_err(MetaError::from)?;
            let next = serials
                .get(JOB_SERIAL_KEY)
                .map_err(MetaError::from)?
                .map_or(0, |value| value.value())
                + 1;
            serials.insert(JOB_SERIAL_KEY, next).map_err(MetaError::from)?;
            format!("jr_{next:016x}")
        };
        let record = JobRunRecord {
            id: id.clone(),
            kind: run.kind,
            scope: run.scope.to_owned(),
            repository: run.repository.map(str::to_owned),
            state: JobState::Running,
            started_at_unix: run.started_at_unix,
            finished_at_unix: None,
            items_processed: 0,
            items_changed: 0,
            error: None,
        };
        {
            let bytes = encode_job_run(&record);
            txn.open_table(JOB_RUN)
                .map_err(MetaError::from)?
                .insert(id.as_str(), bytes.as_slice())
                .map_err(MetaError::from)?;
        }
        txn.commit().map_err(MetaError::from)?;
        Ok(id)
    }

    /// Record the outcome of a job run, returning the updated record when it still exists.
    ///
    /// # Errors
    /// Returns a store error if the write fails or the record cannot be decoded or encoded.
    pub fn finish_job_run(&self, id: &str, outcome: JobOutcome<'_>) -> Result<FinishJobRun, MetaError> {
        let txn = self.db.begin_write()?;
        let Some(mut record) = ({
            let table = txn.open_table(JOB_RUN)?;
            table
                .get(id)?
                .map(|value| serde_json::from_slice::<JobRunRecord>(value.value()))
                .transpose()?
        }) else {
            return Ok(FinishJobRun::Missing);
        };
        if record.state != JobState::Running {
            return Ok(FinishJobRun::AlreadyFinished(record));
        }
        record.state = outcome.state;
        record.finished_at_unix = Some(outcome.finished_at_unix);
        record.items_processed = outcome.items_processed;
        record.items_changed = outcome.items_changed;
        record.error = outcome
            .error
            .map(|error| error[..error.floor_char_boundary(MAX_ERROR_BYTES)].to_owned());
        {
            let bytes = encode_job_run(&record);
            txn.open_table(JOB_RUN)?.insert(id, bytes.as_slice())?;
        }
        txn.commit()?;
        Ok(FinishJobRun::Finished(record))
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

    /// Query job runs newest first with an exclusive stable cursor.
    ///
    /// # Errors
    /// Returns a validation error for an invalid limit or cursor, or a store error if a record cannot
    /// be read or decoded.
    pub fn query_job_runs(&self, query: &JobRunQuery) -> Result<JobRunPage, JobRunQueryError> {
        if !(1..=MAX_QUERY_LIMIT).contains(&query.limit) {
            return Err(JobRunQueryError::InvalidLimit);
        }
        if let Some(cursor) = &query.cursor
            && !valid_job_run_id(cursor)
        {
            return Err(JobRunQueryError::InvalidCursor);
        }
        let mut runs = self.read_job_runs(query.cursor.as_deref(), query.limit + 1)?;
        let next_cursor = (runs.len() > query.limit).then(|| runs[query.limit - 1].id.clone());
        runs.truncate(query.limit);
        Ok(JobRunPage { runs, next_cursor })
    }

    /// Return at most 100 of the newest job runs.
    ///
    /// # Errors
    /// Returns a store error if a record cannot be read or decoded.
    pub fn list_job_runs(&self) -> Result<Vec<JobRunRecord>, MetaError> {
        self.read_job_runs(None, MAX_QUERY_LIMIT)
    }

    /// Mark records left running by an earlier process as failed.
    ///
    /// Each write transaction changes at most 128 records. The method repeats bounded transactions
    /// until no interrupted record remains.
    ///
    /// # Errors
    /// Returns a store error if a record cannot be read, decoded, encoded, or committed.
    pub fn recover_interrupted_job_runs(&self, recovered_at_unix: i64) -> Result<usize, MetaError> {
        let mut recovered = 0;
        loop {
            let changed = self.recover_interrupted_job_run_batch(recovered_at_unix)?;
            recovered += changed;
            if changed < JOB_RETENTION_BATCH {
                return Ok(recovered);
            }
        }
    }

    fn recover_interrupted_job_run_batch(&self, recovered_at_unix: i64) -> Result<usize, MetaError> {
        let txn = self.db.begin_write()?;
        let mut interrupted = Vec::with_capacity(JOB_RETENTION_BATCH);
        {
            let table = txn.open_table(JOB_RUN)?;
            for entry in table.iter()? {
                let (id, value) = entry?;
                let record: JobRunRecord = serde_json::from_slice(value.value())?;
                if record.state == JobState::Running {
                    interrupted.push((id.value().to_owned(), record));
                    if interrupted.len() == JOB_RETENTION_BATCH {
                        break;
                    }
                }
            }
        }
        let changed = interrupted.len();
        {
            let mut table = txn.open_table(JOB_RUN)?;
            for (id, mut record) in interrupted {
                record.state = JobState::Failed;
                record.finished_at_unix = Some(recovered_at_unix);
                record.error = Some(RESTART_ERROR.to_owned());
                let encoded = encode_job_run(&record);
                table.insert(id.as_str(), encoded.as_slice())?;
            }
        }
        txn.commit()?;
        Ok(changed)
    }

    /// Remove one bounded batch of the oldest terminal attempts while retaining every running one.
    ///
    /// # Errors
    /// Returns a store error if history cannot be read, decoded, or committed.
    pub fn prune_job_runs_batch(&self, retain: usize) -> Result<usize, MetaError> {
        let txn = self.db.begin_write()?;
        let removed = prune_terminal_job_runs(&txn, retain)?;
        txn.commit()?;
        Ok(removed)
    }

    fn read_job_runs(&self, cursor: Option<&str>, limit: usize) -> Result<Vec<JobRunRecord>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(JOB_RUN)?;
        let entries = match cursor {
            Some(cursor) => table.range::<&str>((Unbounded, Excluded(cursor)))?,
            None => table.iter()?,
        };
        let mut runs = Vec::with_capacity(limit);
        for entry in entries.rev().take(limit) {
            let (_, value) = entry?;
            runs.push(serde_json::from_slice(value.value())?);
        }
        Ok(runs)
    }
}

fn prune_terminal_job_runs(txn: &redb::WriteTransaction, retain: usize) -> Result<usize, MetaError> {
    let stale = {
        let table = txn.open_table(JOB_RUN)?;
        let count = usize::try_from(table.len()?).unwrap_or(usize::MAX);
        let target = count.saturating_sub(retain).min(JOB_RETENTION_BATCH);
        if target == 0 {
            return Ok(0);
        }
        let mut stale = Vec::with_capacity(target);
        for entry in table.iter()? {
            let (id, value) = entry?;
            let record: JobRunRecord = serde_json::from_slice(value.value())?;
            if record.state != JobState::Running {
                stale.push(id.value().to_owned());
                if stale.len() == target {
                    break;
                }
            }
        }
        stale
    };
    let removed = stale.len();
    let mut table = txn.open_table(JOB_RUN)?;
    for id in stale {
        table.remove(id.as_str())?;
    }
    Ok(removed)
}

fn valid_job_run_id(id: &str) -> bool {
    id.len() == 19
        && id.starts_with("jr_")
        && id[3..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn encode_job_run(record: &JobRunRecord) -> Vec<u8> {
    serde_json::to_vec(record).expect("job run records contain only JSON-compatible values")
}

#[cfg(test)]
#[path = "../../tests/unit/meta/job_fault_tests.rs"]
mod fault_tests;
