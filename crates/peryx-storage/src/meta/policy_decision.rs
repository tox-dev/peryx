use std::collections::{BTreeSet, HashMap, HashSet};

use redb::{ReadableTable as _, ReadableTableMetadata as _};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use peryx_policy::{PolicyAction, PolicyDecisionState};

use super::error::MetaError;
use super::{
    MetaStore, POLICY_DECISION, POLICY_DECISION_CURRENT, POLICY_DECISION_CURRENT_ID, POLICY_DECISION_SERIAL_KEY,
    POLICY_INPUT_GENERATION, SERIAL,
};

const MAX_DECISION_HISTORY: usize = 10_000;
const MAX_QUERY_LIMIT: usize = 100;
const MAX_REASON_BYTES: usize = 2_048;
const MAX_SUBJECT_BYTES: usize = 512;

/// The three inputs a policy result depends on, each counted within one repository.
///
/// `repository` counts changes to that repository's own rows, `catalog` its published catalog
/// identity, and `policy` its configured rules. A result stays fresh while all three still match, so
/// no counter may be shared with another repository.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyInputGeneration {
    pub repository: u64,
    pub catalog: u64,
    pub policy: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct NewPolicyDecision<'a> {
    pub repository: &'a str,
    pub resource: &'a str,
    pub group: Option<&'a str>,
    pub artifact: Option<&'a str>,
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
    pub resource: String,
    pub group: Option<String>,
    pub artifact: Option<String>,
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
    pub resource: Option<String>,
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
            resource: None,
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

impl PolicyDecisionQuery {
    /// # Errors
    /// Returns the first invalid limit, cursor, or text filter.
    pub fn validate(&self) -> Result<(), PolicyDecisionQueryError> {
        if !(1..=MAX_QUERY_LIMIT).contains(&self.limit) {
            return Err(PolicyDecisionQueryError::InvalidLimit);
        }
        if let Some(cursor) = &self.cursor
            && !valid_cursor(cursor)
        {
            return Err(PolicyDecisionQueryError::InvalidCursor);
        }
        for (field, value) in [
            ("repository", self.repository.as_deref()),
            ("resource", self.resource.as_deref()),
            ("rule", self.rule.as_deref()),
            ("source", self.source.as_deref()),
        ] {
            if value.is_some_and(|value| value.len() > MAX_SUBJECT_BYTES) {
                return Err(PolicyDecisionQueryError::FilterTooLong {
                    field,
                    max: MAX_SUBJECT_BYTES,
                });
            }
        }
        Ok(())
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
    resource: &'a str,
    group: Option<&'a str>,
    artifact: Option<&'a str>,
    source: Option<&'a str>,
    action: PolicyAction,
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyDecisionStoreError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("{field} exceeds {max} bytes")]
    FieldTooLong { field: &'static str, max: usize },
    #[error("artifact set exceeds {max} entries")]
    TooManyArtifacts { max: usize },
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyDecisionQueryError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("limit must be between 1 and {MAX_QUERY_LIMIT}")]
    InvalidLimit,
    #[error("invalid policy decision cursor")]
    InvalidCursor,
    #[error("{field} filter exceeds {max} bytes")]
    FilterTooLong { field: &'static str, max: usize },
}

impl MetaStore {
    /// Advances policy inputs without changing catalog identity.
    ///
    /// # Errors
    /// Returns a store error if the generation cannot be read, encoded, or committed.
    pub fn advance_policy_generation(&self, repository: &str) -> Result<PolicyInputGeneration, MetaError> {
        let txn = self.db.begin_write()?;
        let generation = {
            let mut table = txn.open_table(POLICY_INPUT_GENERATION)?;
            let mut generation = table
                .get(repository)?
                .map(|value| serde_json::from_slice::<PolicyInputGeneration>(value.value()))
                .transpose()?
                .unwrap_or_default();
            generation.policy += 1;
            let encoded = serde_json::to_vec(&generation)?;
            table.insert(repository, encoded.as_slice())?;
            generation
        };
        txn.commit()?;
        Ok(generation)
    }

    /// Advances the input revision of each named repository, for a mutation that has already landed
    /// and so cannot advance it in its own transaction.
    ///
    /// A replica applies opaque keys it cannot attribute, so its ecosystem apply hook - which does
    /// parse its own keys - names the repositories here once the rows are committed.
    ///
    /// # Errors
    /// Returns a store error if a generation cannot be read, encoded, or committed.
    pub fn advance_repository_generations(&self, repositories: &BTreeSet<String>) -> Result<(), MetaError> {
        let txn = self.db.begin_write()?;
        advance_repository_generations(&txn, repositories)?;
        txn.commit()?;
        Ok(())
    }

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

    /// Replaces the current result and appends its audit record atomically.
    ///
    /// # Errors
    /// Returns a validation error for an oversized subject or reason, or a store error if the write
    /// cannot be encoded or committed.
    pub fn record_policy_decision(
        &self,
        decision: NewPolicyDecision<'_>,
    ) -> Result<PolicyDecisionRecord, PolicyDecisionStoreError> {
        self.record_policy_decision_with_history_limit(decision, MAX_DECISION_HISTORY)
    }

    fn record_policy_decision_with_history_limit(
        &self,
        decision: NewPolicyDecision<'_>,
        history_limit: usize,
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
        let input_generation = {
            let table = txn.open_table(POLICY_INPUT_GENERATION).map_err(MetaError::from)?;
            table
                .get(decision.repository)
                .map_err(MetaError::from)?
                .map(|value| serde_json::from_slice::<PolicyInputGeneration>(value.value()))
                .transpose()
                .map_err(MetaError::from)?
                .unwrap_or_default()
        };
        let record = PolicyDecisionRecord {
            id: Uuid::new_v4(),
            repository: decision.repository.to_owned(),
            resource: decision.resource.to_owned(),
            group: decision.group.map(str::to_owned),
            artifact: decision.artifact.map(str::to_owned),
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
            let previous = {
                let mut current = txn.open_table(POLICY_DECISION_CURRENT).map_err(MetaError::from)?;
                current
                    .insert(subject.as_str(), history_id.as_str())
                    .map_err(MetaError::from)?
                    .map(|id| id.value().to_owned())
            };
            let mut current_records = txn.open_table(POLICY_DECISION_CURRENT_ID).map_err(MetaError::from)?;
            if let Some(previous) = previous {
                current_records.remove(previous.as_str()).map_err(MetaError::from)?;
            }
            current_records
                .insert(history_id.as_str(), encoded.as_slice())
                .map_err(MetaError::from)?;
        }
        prune_history(&txn, history_limit)?;
        txn.commit().map_err(MetaError::from)?;
        Ok(record)
    }

    /// # Errors
    /// Returns a validation error for an oversized subject, or a store error if the record cannot be
    /// read or decoded.
    ///
    /// # Panics
    /// Panics if a current pointer has no matching record; both tables change in one transaction.
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
        let id = id.value().to_owned();
        let records = txn.open_table(POLICY_DECISION_CURRENT_ID).map_err(MetaError::from)?;
        let record = records
            .get(id.as_str())
            .map_err(MetaError::from)?
            .expect("current policy decision must have a record");
        let record = decode_policy_decision(record.value()).map_err(MetaError::from)?;
        let generations = txn.open_table(POLICY_INPUT_GENERATION).map_err(MetaError::from)?;
        let generation = generations
            .get(record.repository.as_str())
            .map_err(MetaError::from)?
            .map(|value| serde_json::from_slice::<PolicyInputGeneration>(value.value()))
            .transpose()
            .map_err(MetaError::from)?
            .unwrap_or_default();
        Ok((record.input_generation == generation).then_some(record))
    }

    /// Returns the newest current serve or cache decision for each requested artifact.
    ///
    /// # Errors
    /// Returns a validation error for an oversized request or subject, or a store error if a record
    /// cannot be read or decoded.
    pub fn current_policy_decisions_for_artifacts(
        &self,
        repository: &str,
        resource: &str,
        artifacts: &[&str],
    ) -> Result<HashMap<String, PolicyDecisionItem>, PolicyDecisionStoreError> {
        if artifacts.len() > MAX_QUERY_LIMIT {
            return Err(PolicyDecisionStoreError::TooManyArtifacts { max: MAX_QUERY_LIMIT });
        }
        validate_field("repository", repository)?;
        validate_field("resource", resource)?;
        let mut wanted = HashSet::with_capacity(artifacts.len());
        for artifact in artifacts {
            validate_field("artifact", artifact)?;
            wanted.insert(*artifact);
        }
        if wanted.is_empty() {
            return Ok(HashMap::new());
        }

        let txn = self.db.begin_read().map_err(MetaError::from)?;
        let current = txn.open_table(POLICY_DECISION_CURRENT_ID).map_err(MetaError::from)?;
        let generations = txn.open_table(POLICY_INPUT_GENERATION).map_err(MetaError::from)?;
        let generation = generations
            .get(repository)
            .map_err(MetaError::from)?
            .map(|value| serde_json::from_slice::<PolicyInputGeneration>(value.value()))
            .transpose()
            .map_err(MetaError::from)?
            .unwrap_or_default();
        let mut decisions = HashMap::with_capacity(wanted.len());
        for entry in current.iter().map_err(MetaError::from)?.rev() {
            let (_, value) = entry.map_err(MetaError::from)?;
            let record = decode_policy_decision(value.value()).map_err(MetaError::from)?;
            if record.repository != repository
                || record.resource != resource
                || !matches!(record.action, PolicyAction::Serve | PolicyAction::Cached)
            {
                continue;
            }
            let Some(artifact) = record.artifact.as_deref() else {
                continue;
            };
            if !wanted.contains(artifact) || decisions.contains_key(artifact) {
                continue;
            }
            decisions.insert(
                artifact.to_owned(),
                PolicyDecisionItem {
                    fresh: record.input_generation == generation,
                    record,
                },
            );
            if decisions.len() == wanted.len() {
                break;
            }
        }
        Ok(decisions)
    }

    /// Returns decision history newest first after an exclusive stable cursor.
    ///
    /// # Errors
    /// Returns a validation error for an invalid limit, cursor, or oversized text filter, or a store
    /// error if a record cannot be read or decoded.
    pub fn query_policy_decisions(
        &self,
        query: &PolicyDecisionQuery,
    ) -> Result<PolicyDecisionPage, PolicyDecisionQueryError> {
        query.validate()?;
        let txn = self.db.begin_read().map_err(MetaError::from)?;
        let history = txn.open_table(POLICY_DECISION).map_err(MetaError::from)?;
        let generations = txn.open_table(POLICY_INPUT_GENERATION).map_err(MetaError::from)?;
        let mut decisions = Vec::with_capacity(query.limit + 1);
        let mut cursors = Vec::with_capacity(query.limit + 1);
        for entry in history.iter().map_err(MetaError::from)?.rev() {
            let (id, value) = entry.map_err(MetaError::from)?;
            if query.cursor.as_deref().is_some_and(|cursor| id.value() >= cursor) {
                continue;
            }
            let record = decode_policy_decision(value.value()).map_err(MetaError::from)?;
            if !matches_query(&record, query) {
                continue;
            }
            let generation = generations
                .get(record.repository.as_str())
                .map_err(MetaError::from)?
                .map(|value| serde_json::from_slice::<PolicyInputGeneration>(value.value()))
                .transpose()
                .map_err(MetaError::from)?
                .unwrap_or_default();
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

pub(super) fn advance_repository_generations(
    txn: &redb::WriteTransaction,
    repositories: &BTreeSet<String>,
) -> Result<(), MetaError> {
    if repositories.is_empty() {
        return Ok(());
    }
    let mut generations = txn.open_table(POLICY_INPUT_GENERATION)?;
    for repository in repositories {
        let mut generation = generations
            .get(repository.as_str())?
            .map(|value| serde_json::from_slice::<PolicyInputGeneration>(value.value()))
            .transpose()?
            .unwrap_or_default();
        generation.repository += 1;
        let encoded = serde_json::to_vec(&generation)?;
        generations.insert(repository.as_str(), encoded.as_slice())?;
    }
    Ok(())
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
        ("resource", Some(decision.resource)),
        ("group", decision.group),
        ("artifact", decision.artifact),
        ("source", decision.source),
    ] {
        if let Some(value) = value {
            validate_field(field, value)?;
        }
    }
    Ok(())
}

const fn validate_field(field: &'static str, value: &str) -> Result<(), PolicyDecisionStoreError> {
    if value.len() > MAX_SUBJECT_BYTES {
        return Err(PolicyDecisionStoreError::FieldTooLong {
            field,
            max: MAX_SUBJECT_BYTES,
        });
    }
    Ok(())
}

fn subject_key(decision: &NewPolicyDecision<'_>) -> Result<String, serde_json::Error> {
    serde_json::to_string(&PolicyDecisionSubject {
        repository: decision.repository,
        resource: decision.resource,
        group: decision.group,
        artifact: decision.artifact,
        source: decision.source,
        action: decision.action,
    })
}

fn decode_policy_decision(bytes: &[u8]) -> Result<PolicyDecisionRecord, serde_json::Error> {
    serde_json::from_slice(bytes)
}

/// Drops the oldest audit row once the log crosses its bound.
///
/// Current decisions live in their own table, so an evicted row never takes a subject's live state
/// with it - not even a subject in another repository, whose rows interleave with these by serial.
fn prune_history(txn: &redb::WriteTransaction, history_limit: usize) -> Result<(), MetaError> {
    let stale_id = {
        let history = txn.open_table(POLICY_DECISION)?;
        (history.len()? > history_limit as u64)
            .then(|| history.first())
            .transpose()?
            .flatten()
            .map(|(id, _)| id.value().to_owned())
    };
    let Some(stale_id) = stale_id else {
        return Ok(());
    };
    txn.open_table(POLICY_DECISION)?.remove(stale_id.as_str())?;
    Ok(())
}

fn matches_query(record: &PolicyDecisionRecord, query: &PolicyDecisionQuery) -> bool {
    query
        .repository
        .as_deref()
        .is_none_or(|repository| record.repository == repository)
        && query
            .resource
            .as_deref()
            .is_none_or(|resource| record.resource == resource)
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

#[cfg(test)]
#[path = "../../tests/unit/meta/policy_decision_fault_tests.rs"]
mod fault_tests;

#[cfg(test)]
#[path = "../../tests/unit/meta/policy_decision_tests.rs"]
mod tests;
