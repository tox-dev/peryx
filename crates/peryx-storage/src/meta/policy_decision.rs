use redb::{ReadableDatabase as _, ReadableTable as _, ReadableTableMetadata as _};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use peryx_policy::{PolicyAction, PolicyDecisionState};

use super::error::MetaError;
use super::{
    MetaStore, POLICY_DECISION, POLICY_DECISION_CURRENT, POLICY_DECISION_CURRENT_ID, POLICY_DECISION_SERIAL_KEY,
    POLICY_INPUT_GENERATION, SERIAL, SERIAL_KEY,
};

#[cfg(not(test))]
const MAX_DECISION_HISTORY: usize = 10_000;
#[cfg(test)]
const MAX_DECISION_HISTORY: usize = 16;
const MAX_QUERY_LIMIT: usize = 100;
const MAX_REASON_BYTES: usize = 2_048;
const MAX_SUBJECT_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyInputGeneration {
    pub repository: u64,
    pub catalog: u64,
    pub policy: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct NewPolicyDecision<'a> {
    pub repository: &'a str,
    pub project: &'a str,
    pub version: Option<&'a str>,
    pub filename: Option<&'a str>,
    pub source: Option<&'a str>,
    pub action: PolicyAction,
    pub state: PolicyDecisionState,
    pub rule: Option<&'a str>,
    pub reason: Option<&'a str>,
    pub evaluated_at_unix: i64,
    pub next_eligible_at_unix: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDecisionRecord {
    pub id: Uuid,
    pub repository: String,
    pub project: String,
    pub version: Option<String>,
    pub filename: Option<String>,
    pub source: Option<String>,
    pub action: PolicyAction,
    pub state: PolicyDecisionState,
    pub rule: Option<String>,
    pub reason: Option<String>,
    pub evaluated_at_unix: i64,
    pub input_generation: PolicyInputGeneration,
    pub next_eligible_at_unix: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PolicyDecisionItem {
    #[serde(flatten)]
    pub record: PolicyDecisionRecord,
    pub fresh: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyDecisionQuery {
    pub repository: Option<String>,
    pub state: Option<PolicyDecisionState>,
    pub rule: Option<String>,
    pub source: Option<String>,
    pub evaluated_from_unix: Option<i64>,
    pub evaluated_to_unix: Option<i64>,
    pub cursor: Option<String>,
    pub limit: usize,
}

impl Default for PolicyDecisionQuery {
    fn default() -> Self {
        Self {
            repository: None,
            state: None,
            rule: None,
            source: None,
            evaluated_from_unix: None,
            evaluated_to_unix: None,
            cursor: None,
            limit: 25,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PolicyDecisionPage {
    pub decisions: Vec<PolicyDecisionItem>,
    pub next_cursor: Option<String>,
}

#[derive(Serialize)]
struct PolicyDecisionSubject<'a> {
    repository: &'a str,
    project: &'a str,
    version: Option<&'a str>,
    filename: Option<&'a str>,
    source: Option<&'a str>,
    action: PolicyAction,
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyDecisionStoreError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("{field} exceeds {max} bytes")]
    FieldTooLong { field: &'static str, max: usize },
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyDecisionQueryError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("limit must be between 1 and {MAX_QUERY_LIMIT}")]
    InvalidLimit,
    #[error("invalid policy decision cursor")]
    InvalidCursor,
}

impl MetaStore {
    /// Advance one repository's policy generation without changing its catalog identity.
    ///
    /// # Errors
    /// Returns a store error if the generation cannot be read, encoded, or committed.
    pub fn advance_policy_generation(&self, repository: &str) -> Result<PolicyInputGeneration, MetaError> {
        let txn = self.db.begin_write()?;
        let repository_generation = txn
            .open_table(SERIAL)?
            .get(SERIAL_KEY)?
            .map_or(0, |value| value.value());
        let generation = {
            let mut table = txn.open_table(POLICY_INPUT_GENERATION)?;
            let mut generation = table
                .get(repository)?
                .map(|value| serde_json::from_slice::<PolicyInputGeneration>(value.value()))
                .transpose()?
                .unwrap_or_default();
            generation.repository = repository_generation;
            generation.policy += 1;
            let encoded = serde_json::to_vec(&generation)?;
            table.insert(repository, encoded.as_slice())?;
            generation
        };
        txn.commit()?;
        Ok(generation)
    }

    /// Return the current policy inputs for one repository.
    ///
    /// # Errors
    /// Returns a store error if the generation cannot be read or decoded.
    pub fn policy_input_generation(&self, repository: &str) -> Result<PolicyInputGeneration, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(POLICY_INPUT_GENERATION)?;
        Ok(table
            .get(repository)?
            .map(|value| serde_json::from_slice(value.value()))
            .transpose()?
            .unwrap_or_default())
    }

    /// Replace the current subject-action result and append its bounded audit record atomically.
    ///
    /// # Errors
    /// Returns a validation error for an oversized subject or reason, or a store error if the write
    /// cannot be encoded or committed.
    pub fn record_policy_decision(
        &self,
        decision: NewPolicyDecision<'_>,
    ) -> Result<PolicyDecisionRecord, PolicyDecisionStoreError> {
        validate_decision(&decision)?;
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        let history_id = {
            let mut serials = txn.open_table(SERIAL).map_err(MetaError::from)?;
            let next = serials
                .get(POLICY_DECISION_SERIAL_KEY)
                .map_err(MetaError::from)?
                .map_or(0, |value| value.value())
                + 1;
            serials
                .insert(POLICY_DECISION_SERIAL_KEY, next)
                .map_err(MetaError::from)?;
            format!("pd_{next:016x}")
        };
        let mut input_generation = {
            let table = txn.open_table(POLICY_INPUT_GENERATION).map_err(MetaError::from)?;
            table
                .get(decision.repository)
                .map_err(MetaError::from)?
                .map(|value| serde_json::from_slice::<PolicyInputGeneration>(value.value()))
                .transpose()
                .map_err(MetaError::from)?
                .unwrap_or_default()
        };
        input_generation.repository = txn
            .open_table(SERIAL)
            .map_err(MetaError::from)?
            .get(SERIAL_KEY)
            .map_err(MetaError::from)?
            .map_or(0, |value| value.value());
        let record = PolicyDecisionRecord {
            id: Uuid::new_v4(),
            repository: decision.repository.to_owned(),
            project: decision.project.to_owned(),
            version: decision.version.map(str::to_owned),
            filename: decision.filename.map(str::to_owned),
            source: decision.source.map(str::to_owned),
            action: decision.action,
            state: decision.state,
            rule: decision.rule.map(str::to_owned),
            reason: decision.reason.map(str::to_owned),
            evaluated_at_unix: decision.evaluated_at_unix,
            input_generation,
            next_eligible_at_unix: decision.next_eligible_at_unix,
        };
        let subject = subject_key(&decision).map_err(MetaError::from)?;
        let encoded = serde_json::to_vec(&record).map_err(MetaError::from)?;
        {
            txn.open_table(POLICY_DECISION)
                .map_err(MetaError::from)?
                .insert(history_id.as_str(), encoded.as_slice())
                .map_err(MetaError::from)?;
            let previous = txn
                .open_table(POLICY_DECISION_CURRENT)
                .map_err(MetaError::from)?
                .insert(subject.as_str(), history_id.as_str())
                .map_err(MetaError::from)?
                .map(|id| id.value().to_owned());
            let mut current_ids = txn.open_table(POLICY_DECISION_CURRENT_ID).map_err(MetaError::from)?;
            if let Some(previous) = previous {
                current_ids.remove(previous.as_str()).map_err(MetaError::from)?;
            }
            current_ids
                .insert(history_id.as_str(), subject.as_str())
                .map_err(MetaError::from)?;
        }
        prune_history(&txn)?;
        txn.commit().map_err(MetaError::from)?;
        Ok(record)
    }

    /// Return the current fresh decision for one subject and action.
    ///
    /// # Errors
    /// Returns a validation error for an oversized subject, or a store error if the record cannot be
    /// read or decoded.
    ///
    /// # Panics
    /// Panics if a current pointer has no matching history record; both tables change in one
    /// transaction.
    pub fn current_policy_decision(
        &self,
        subject: NewPolicyDecision<'_>,
    ) -> Result<Option<PolicyDecisionRecord>, PolicyDecisionStoreError> {
        validate_subject(&subject)?;
        let txn = self.db.begin_read().map_err(MetaError::from)?;
        let current = txn.open_table(POLICY_DECISION_CURRENT).map_err(MetaError::from)?;
        let key = subject_key(&subject).map_err(MetaError::from)?;
        let Some(id) = current.get(key.as_str()).map_err(MetaError::from)? else {
            return Ok(None);
        };
        let history = txn.open_table(POLICY_DECISION).map_err(MetaError::from)?;
        let record = history
            .get(id.value())
            .map_err(MetaError::from)?
            .expect("current policy decision must have history");
        let record: PolicyDecisionRecord = serde_json::from_slice(record.value()).map_err(MetaError::from)?;
        let generations = txn.open_table(POLICY_INPUT_GENERATION).map_err(MetaError::from)?;
        let mut generation = generations
            .get(record.repository.as_str())
            .map_err(MetaError::from)?
            .map(|value| serde_json::from_slice::<PolicyInputGeneration>(value.value()))
            .transpose()
            .map_err(MetaError::from)?
            .unwrap_or_default();
        generation.repository = txn
            .open_table(SERIAL)
            .map_err(MetaError::from)?
            .get(SERIAL_KEY)
            .map_err(MetaError::from)?
            .map_or(0, |value| value.value());
        Ok((record.input_generation == generation).then_some(record))
    }

    /// Query bounded decision history newest first with an exclusive stable cursor.
    ///
    /// # Errors
    /// Returns a validation error for an invalid limit or cursor, or a store error if a record cannot
    /// be read or decoded.
    pub fn query_policy_decisions(
        &self,
        query: &PolicyDecisionQuery,
    ) -> Result<PolicyDecisionPage, PolicyDecisionQueryError> {
        if !(1..=MAX_QUERY_LIMIT).contains(&query.limit) {
            return Err(PolicyDecisionQueryError::InvalidLimit);
        }
        if let Some(cursor) = &query.cursor
            && !valid_cursor(cursor)
        {
            return Err(PolicyDecisionQueryError::InvalidCursor);
        }
        let txn = self.db.begin_read().map_err(MetaError::from)?;
        let history = txn.open_table(POLICY_DECISION).map_err(MetaError::from)?;
        let generations = txn.open_table(POLICY_INPUT_GENERATION).map_err(MetaError::from)?;
        let repository_generation = txn
            .open_table(SERIAL)
            .map_err(MetaError::from)?
            .get(SERIAL_KEY)
            .map_err(MetaError::from)?
            .map_or(0, |value| value.value());
        let mut decisions = Vec::with_capacity(query.limit + 1);
        let mut cursors = Vec::with_capacity(query.limit + 1);
        for entry in history.iter().map_err(MetaError::from)?.rev() {
            let (id, value) = entry.map_err(MetaError::from)?;
            if query.cursor.as_deref().is_some_and(|cursor| id.value() >= cursor) {
                continue;
            }
            let record: PolicyDecisionRecord = serde_json::from_slice(value.value()).map_err(MetaError::from)?;
            if !matches_query(&record, query) {
                continue;
            }
            let mut generation = generations
                .get(record.repository.as_str())
                .map_err(MetaError::from)?
                .map(|value| serde_json::from_slice::<PolicyInputGeneration>(value.value()))
                .transpose()
                .map_err(MetaError::from)?
                .unwrap_or_default();
            generation.repository = repository_generation;
            decisions.push(PolicyDecisionItem {
                fresh: record.input_generation == generation,
                record,
            });
            cursors.push(id.value().to_owned());
            if decisions.len() > query.limit {
                break;
            }
        }
        let next_cursor = (decisions.len() > query.limit).then(|| cursors[query.limit - 1].clone());
        decisions.truncate(query.limit);
        Ok(PolicyDecisionPage { decisions, next_cursor })
    }
}

fn validate_decision(decision: &NewPolicyDecision<'_>) -> Result<(), PolicyDecisionStoreError> {
    validate_subject(decision)?;
    if decision.rule.is_some_and(|rule| rule.len() > MAX_SUBJECT_BYTES) {
        return Err(PolicyDecisionStoreError::FieldTooLong {
            field: "rule",
            max: MAX_SUBJECT_BYTES,
        });
    }
    if decision.reason.is_some_and(|reason| reason.len() > MAX_REASON_BYTES) {
        return Err(PolicyDecisionStoreError::FieldTooLong {
            field: "reason",
            max: MAX_REASON_BYTES,
        });
    }
    Ok(())
}

fn validate_subject(decision: &NewPolicyDecision<'_>) -> Result<(), PolicyDecisionStoreError> {
    for (field, value) in [
        ("repository", Some(decision.repository)),
        ("project", Some(decision.project)),
        ("version", decision.version),
        ("filename", decision.filename),
        ("source", decision.source),
    ] {
        if value.is_some_and(|value| value.len() > MAX_SUBJECT_BYTES) {
            return Err(PolicyDecisionStoreError::FieldTooLong {
                field,
                max: MAX_SUBJECT_BYTES,
            });
        }
    }
    Ok(())
}

fn subject_key(decision: &NewPolicyDecision<'_>) -> Result<String, serde_json::Error> {
    serde_json::to_string(&PolicyDecisionSubject {
        repository: decision.repository,
        project: decision.project,
        version: decision.version,
        filename: decision.filename,
        source: decision.source,
        action: decision.action,
    })
}

fn prune_history(txn: &redb::WriteTransaction) -> Result<(), PolicyDecisionStoreError> {
    let stale_id = {
        let history = txn.open_table(POLICY_DECISION).map_err(MetaError::from)?;
        (history.len().map_err(MetaError::from)? > MAX_DECISION_HISTORY as u64)
            .then(|| history.first())
            .transpose()
            .map_err(MetaError::from)?
            .flatten()
            .map(|(id, _)| id.value().to_owned())
    };
    let Some(stale_id) = stale_id else {
        return Ok(());
    };
    let stale_subject = {
        let current_ids = txn.open_table(POLICY_DECISION_CURRENT_ID).map_err(MetaError::from)?;
        current_ids
            .get(stale_id.as_str())
            .map_err(MetaError::from)?
            .map(|subject| subject.value().to_owned())
    };
    txn.open_table(POLICY_DECISION)
        .map_err(MetaError::from)?
        .remove(stale_id.as_str())
        .map_err(MetaError::from)?;
    if let Some(subject) = stale_subject {
        txn.open_table(POLICY_DECISION_CURRENT)
            .map_err(MetaError::from)?
            .remove(subject.as_str())
            .map_err(MetaError::from)?;
        txn.open_table(POLICY_DECISION_CURRENT_ID)
            .map_err(MetaError::from)?
            .remove(stale_id.as_str())
            .map_err(MetaError::from)?;
    }
    Ok(())
}

fn matches_query(record: &PolicyDecisionRecord, query: &PolicyDecisionQuery) -> bool {
    query
        .repository
        .as_deref()
        .is_none_or(|repository| record.repository == repository)
        && query.state.is_none_or(|state| record.state == state)
        && query
            .rule
            .as_deref()
            .is_none_or(|rule| record.rule.as_deref() == Some(rule))
        && query
            .source
            .as_deref()
            .is_none_or(|source| record.source.as_deref() == Some(source))
        && query
            .evaluated_from_unix
            .is_none_or(|start| record.evaluated_at_unix >= start)
        && query
            .evaluated_to_unix
            .is_none_or(|end| record.evaluated_at_unix <= end)
}

fn valid_cursor(cursor: &str) -> bool {
    cursor.len() == 19 && cursor.starts_with("pd_") && cursor[3..].bytes().all(|byte| byte.is_ascii_hexdigit())
}
