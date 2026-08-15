//! Durable write intents survive ingress restarts and deduplicate retries by client-scoped key and content.
//! Per-authority limits prevent one resource from exhausting the ledger; a durable sequence preserves
//! admission order. Payloads remain opaque to storage.

use redb::{ReadableTable as _, ReadableTableMetadata as _};
use serde::{Deserialize, Serialize};

use super::{
    INGRESS_INTENT, INGRESS_INTENT_COUNT, INGRESS_INTENT_ORDER, INGRESS_INTENT_SEQ, INGRESS_SEQ_KEY, MetaError,
    MetaStore, open_optional_table,
};

/// Declaration order enforces forward-only lifecycle transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentPhase {
    /// Awaiting home-DC finalization.
    Pending,
    /// Finalized at the home DC.
    Admitted,
    /// Eligible for reclamation after its retention window.
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedIntent {
    pub phase: IntentPhase,
    /// Allows per-authority accounting without parsing the opaque key.
    pub authority: String,
    /// Durable order-index key.
    pub seq: u64,
    /// Distinguishes a duplicate retry from a conflicting reuse of the key.
    pub digest: String,
    pub size: u64,
    /// Replayed verbatim without interpretation.
    pub payload: Vec<u8>,
    pub updated_at_unix: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntentLimits {
    pub max_records: u64,
    pub max_bytes: u64,
    /// Soft threshold for either hard limit.
    pub backpressure_percent: u8,
}

impl IntentLimits {
    const fn soft_records(self) -> u64 {
        self.max_records.saturating_mul(self.backpressure_percent as u64) / 100
    }

    const fn soft_bytes(self) -> u64 {
        self.max_bytes.saturating_mul(self.backpressure_percent as u64) / 100
    }

    const fn is_backpressured(self, usage: IntentUsage) -> bool {
        usage.records >= self.soft_records() || usage.bytes >= self.soft_bytes()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct IntentUsage {
    pub records: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackpressureState {
    Nominal,
    /// The caller should shed load before reaching a hard limit.
    Backpressured,
}

#[derive(Debug, Clone, Copy)]
pub struct IntentAdmission<'a> {
    pub authority: &'a str,
    pub key: &'a str,
    pub digest: &'a str,
    pub size: u64,
    pub payload: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentStageOutcome {
    Admitted,
    /// The first admission remains unchanged.
    Duplicate,
    /// The key already binds different content.
    Conflict,
    RejectedOverRecordLimit,
    RejectedOverByteLimit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntentStageResult {
    pub outcome: IntentStageOutcome,
    pub pressure: BackpressureState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentTransition {
    Advanced,
    /// Includes missing intents and non-forward transitions.
    Ignored,
}

impl MetaStore {
    /// Atomically deduplicates by key, enforces per-authority hard limits, and reports post-admission
    /// backpressure. A key bound to different content returns [`IntentStageOutcome::Conflict`].
    ///
    /// # Errors
    /// Returns a store error when the row cannot be read, encoded, or committed.
    pub fn stage_intent(
        &self,
        admission: IntentAdmission<'_>,
        limits: IntentLimits,
        now: i64,
    ) -> Result<IntentStageResult, MetaError> {
        let txn = self.db.begin_write()?;
        let result;
        {
            let mut table = txn.open_table(INGRESS_INTENT)?;
            let mut counts = txn.open_table(INGRESS_INTENT_COUNT)?;
            let mut order = txn.open_table(INGRESS_INTENT_ORDER)?;
            let mut sequence = txn.open_table(INGRESS_INTENT_SEQ)?;
            let existing = table
                .get(admission.key)?
                .map(|value| serde_json::from_slice::<StagedIntent>(value.value()))
                .transpose()?;
            let usage = read_usage(&counts, admission.authority)?;
            let outcome = match existing {
                Some(record) if record.digest == admission.digest && record.size == admission.size => {
                    IntentStageOutcome::Duplicate
                }
                Some(_) => IntentStageOutcome::Conflict,
                None if usage.records + 1 > limits.max_records => IntentStageOutcome::RejectedOverRecordLimit,
                None if usage.bytes.saturating_add(admission.size) > limits.max_bytes => {
                    IntentStageOutcome::RejectedOverByteLimit
                }
                None => {
                    let seq = sequence.get(INGRESS_SEQ_KEY)?.map_or(0, |value| value.value());
                    sequence.insert(INGRESS_SEQ_KEY, seq + 1)?;
                    let record = StagedIntent {
                        phase: IntentPhase::Pending,
                        authority: admission.authority.to_owned(),
                        seq,
                        digest: admission.digest.to_owned(),
                        size: admission.size,
                        payload: admission.payload.to_vec(),
                        updated_at_unix: now,
                    };
                    table.insert(admission.key, serde_json::to_vec(&record)?.as_slice())?;
                    order.insert(seq, admission.key)?;
                    let admitted = IntentUsage {
                        records: usage.records + 1,
                        bytes: usage.bytes + admission.size,
                    };
                    counts.insert(admission.authority, serde_json::to_vec(&admitted)?.as_slice())?;
                    IntentStageOutcome::Admitted
                }
            };
            let settled = if outcome == IntentStageOutcome::Admitted {
                IntentUsage {
                    records: usage.records + 1,
                    bytes: usage.bytes + admission.size,
                }
            } else {
                usage
            };
            let pressure = if limits.is_backpressured(settled) {
                BackpressureState::Backpressured
            } else {
                BackpressureState::Nominal
            };
            result = IntentStageResult { outcome, pressure };
        }
        txn.commit()?;
        Ok(result)
    }

    /// Applies only forward transitions. Settled intents continue using capacity until pruning.
    ///
    /// # Errors
    /// Returns a store error when the row cannot be read, encoded, or committed.
    pub fn advance_intent(&self, intent_key: &str, to: IntentPhase, now: i64) -> Result<IntentTransition, MetaError> {
        let txn = self.db.begin_write()?;
        let outcome;
        {
            let mut table = txn.open_table(INGRESS_INTENT)?;
            let existing = table
                .get(intent_key)?
                .map(|value| serde_json::from_slice::<StagedIntent>(value.value()))
                .transpose()?;
            outcome = match existing {
                Some(mut record) if to > record.phase => {
                    record.phase = to;
                    record.updated_at_unix = now;
                    table.insert(intent_key, serde_json::to_vec(&record)?.as_slice())?;
                    IntentTransition::Advanced
                }
                _ => IntentTransition::Ignored,
            };
        }
        txn.commit()?;
        Ok(outcome)
    }

    /// Returns `None` when no intent is retained under `intent_key`.
    ///
    /// # Errors
    /// Returns a store error when the row cannot be read or decoded.
    pub fn staged_intent(&self, intent_key: &str) -> Result<Option<StagedIntent>, MetaError> {
        let txn = self.db.begin_read()?;
        let Some(table) = open_optional_table(&txn, INGRESS_INTENT)? else {
            return Ok(None);
        };
        Ok(table
            .get(intent_key)?
            .map(|value| serde_json::from_slice(value.value()))
            .transpose()?)
    }

    /// # Errors
    /// Returns a store error when the counter row cannot be read or decoded.
    pub fn staged_intent_usage(&self, authority: &str) -> Result<IntentUsage, MetaError> {
        let txn = self.db.begin_read()?;
        let Some(counts) = open_optional_table(&txn, INGRESS_INTENT_COUNT)? else {
            return Ok(IntentUsage::default());
        };
        read_usage(&counts, authority)
    }

    /// Returns up to `limit` pending intents in durable admission order. Settled order entries remain
    /// indexed until pruning but do not appear.
    ///
    /// # Errors
    /// Returns a store error when a table cannot be read or a record decoded.
    pub fn list_pending_intents(&self, limit: usize) -> Result<Vec<(String, StagedIntent)>, MetaError> {
        let txn = self.db.begin_read()?;
        let Some(order) = open_optional_table(&txn, INGRESS_INTENT_ORDER)? else {
            return Ok(Vec::new());
        };
        let table = txn.open_table(INGRESS_INTENT)?;
        let mut pending = Vec::new();
        for entry in order.iter()? {
            let (_, key) = entry?;
            let key = key.value();
            let Some(value) = table.get(key)? else { continue };
            let record: StagedIntent = serde_json::from_slice(value.value())?;
            if record.phase == IntentPhase::Pending {
                pending.push((key.to_owned(), record));
                if pending.len() == limit {
                    break;
                }
            }
        }
        Ok(pending)
    }

    /// # Errors
    /// Returns a store error when the table cannot be read.
    pub fn count_staged_intents(&self) -> Result<u64, MetaError> {
        let txn = self.db.begin_read()?;
        let Some(table) = open_optional_table(&txn, INGRESS_INTENT)? else {
            return Ok(0);
        };
        Ok(table.len()?)
    }

    /// Removes up to `limit` settled intents past retention and releases their authority capacity. Pending
    /// work remains eligible to finalize and is never pruned.
    ///
    /// # Errors
    /// Returns a store error when a row cannot be read or the delete cannot be committed.
    pub fn prune_ingress_intents(&self, now: i64, retention_secs: i64, limit: usize) -> Result<usize, MetaError> {
        let txn = self.db.begin_write()?;
        let pruned;
        {
            let mut table = txn.open_table(INGRESS_INTENT)?;
            let mut counts = txn.open_table(INGRESS_INTENT_COUNT)?;
            let mut order = txn.open_table(INGRESS_INTENT_ORDER)?;
            let mut doomed = Vec::new();
            for entry in table.iter()? {
                if doomed.len() >= limit {
                    break;
                }
                let (key, value) = entry?;
                let record: StagedIntent = serde_json::from_slice(value.value())?;
                if matches!(record.phase, IntentPhase::Admitted | IntentPhase::Expired)
                    && now >= record.updated_at_unix + retention_secs
                {
                    doomed.push((key.value().to_owned(), record));
                }
            }
            for (key, record) in &doomed {
                table.remove(key.as_str())?;
                order.remove(record.seq)?;
                let usage = read_usage(&counts, &record.authority)?;
                let remaining = IntentUsage {
                    records: usage.records.saturating_sub(1),
                    bytes: usage.bytes.saturating_sub(record.size),
                };
                if remaining.records == 0 {
                    counts.remove(record.authority.as_str())?;
                } else {
                    counts.insert(record.authority.as_str(), serde_json::to_vec(&remaining)?.as_slice())?;
                }
            }
            pruned = doomed.len();
        }
        txn.commit()?;
        Ok(pruned)
    }
}

fn read_usage(
    counts: &impl redb::ReadableTable<&'static str, &'static [u8]>,
    authority: &str,
) -> Result<IntentUsage, MetaError> {
    Ok(counts
        .get(authority)?
        .map(|value| serde_json::from_slice(value.value()))
        .transpose()?
        .unwrap_or_default())
}
