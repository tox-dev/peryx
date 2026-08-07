//! The ingress DC's durable staging ledger for write intents.
//!
//! The replication layer holds the pure admission state; this persists it so a restart recovers the
//! intents a home DC has yet to finalize. Each intent is keyed by its opaque client-scoped identity and
//! carries the content it binds to - a digest and byte size - so a retried admission of the same key is
//! idempotent: the same content is a duplicate, and different content a conflict that never overwrites
//! the bytes a home DC will finalize.
//!
//! The buffer is bounded per authority, not globally: each authority stages up to a record and a byte
//! ceiling, so one busy project cannot starve the ledger of every other. Admission trips backpressure at a
//! soft fraction of the ceiling before it refuses at the hard bound, giving a stalled home DC a shed
//! signal ahead of an outright rejection. Every admission takes the next value of a durable, never-reused
//! sequence, and an order index keys the pending set by it, so a restart resumes the drain in the exact
//! order writes were admitted rather than in key order.
//!
//! The staged payload the caller serializes is opaque here, like the neutral driver key-value table: the
//! store reads only the key, the content binding, the authority scope, and the lifecycle phase.

use redb::{ReadableTable as _, ReadableTableMetadata as _};
use serde::{Deserialize, Serialize};

use super::{
    INGRESS_INTENT, INGRESS_INTENT_COUNT, INGRESS_INTENT_ORDER, INGRESS_INTENT_SEQ, INGRESS_SEQ_KEY, MetaError,
    MetaStore,
};

/// Where a staged intent sits in its lifecycle. Declared in advancing order, so the derived ordering
/// ranks `Pending` below `Admitted` below `Expired` and a transition only moves forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentPhase {
    /// Admitted at the ingress DC and retained, awaiting home-DC finalization.
    Pending,
    /// The home DC finalized the write; the intent is settled.
    Admitted,
    /// The retention window elapsed before finalization; the intent is eligible for reclamation.
    Expired,
}

/// One durably staged write intent: its lifecycle phase, the authority it counts against, the durable
/// admission sequence it took, the content it binds to, and the opaque payload the caller serialized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedIntent {
    pub phase: IntentPhase,
    /// The authority the intent counts against, so the store bounds and prunes per authority without
    /// parsing the opaque key.
    pub authority: String,
    /// The durable admission sequence this intent took, the key the order index sorts the pending set by.
    pub seq: u64,
    /// The content digest the intent commits to, used to tell a duplicate retry from a conflict.
    pub digest: String,
    pub size: u64,
    /// The intent the caller serialized, replayed verbatim; the store never interprets it.
    pub payload: Vec<u8>,
    pub updated_at_unix: i64,
}

/// The per-authority admission ceilings and the fraction of them at which backpressure trips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntentLimits {
    /// The most retained intents one authority may hold before a new one is refused.
    pub max_records: u64,
    /// The most retained bytes one authority may hold before a new one is refused.
    pub max_bytes: u64,
    /// The percent of either ceiling at which admission reports backpressure ahead of the hard bound.
    pub backpressure_percent: u8,
}

impl IntentLimits {
    /// The record count at or above which admission reports backpressure.
    const fn soft_records(self) -> u64 {
        self.max_records.saturating_mul(self.backpressure_percent as u64) / 100
    }

    /// The byte count at or above which admission reports backpressure.
    const fn soft_bytes(self) -> u64 {
        self.max_bytes.saturating_mul(self.backpressure_percent as u64) / 100
    }

    /// Whether `usage` sits at or above either soft threshold.
    const fn is_backpressured(self, usage: IntentUsage) -> bool {
        usage.records >= self.soft_records() || usage.bytes >= self.soft_bytes()
    }
}

/// The retained records and bytes one authority currently holds, the per-authority counter the ceilings
/// bound and the retained-capacity metric reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct IntentUsage {
    pub records: u64,
    pub bytes: u64,
}

/// Whether admission is running clear or has crossed the soft threshold ahead of the hard bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackpressureState {
    /// The authority's retained usage is below both soft thresholds.
    Nominal,
    /// The authority's retained usage is at or above a soft threshold, so the caller should shed early.
    Backpressured,
}

/// The identity and content one admission stages.
///
/// It carries the authority the intent counts against, the client-scoped key it deduplicates on, and the
/// content binding plus opaque payload it retains for the finalization path.
#[derive(Debug, Clone, Copy)]
pub struct IntentAdmission<'a> {
    pub authority: &'a str,
    pub key: &'a str,
    pub digest: &'a str,
    pub size: u64,
    pub payload: &'a [u8],
}

/// The verdict of staging an intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentStageOutcome {
    /// A new distinct intent was admitted and retained as [`Pending`](IntentPhase::Pending).
    Admitted,
    /// The key was already staged for the same content, so the first admission stands.
    Duplicate,
    /// The key was already staged for different content, so it is refused rather than overwriting it.
    Conflict,
    /// The authority already holds its record ceiling, so a new intent is refused.
    RejectedOverRecordLimit,
    /// Admitting the intent would push the authority past its byte ceiling, so it is refused.
    RejectedOverByteLimit,
}

/// The outcome of staging an intent together with the backpressure signal the authority is under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntentStageResult {
    pub outcome: IntentStageOutcome,
    pub pressure: BackpressureState,
}

/// Whether a lifecycle transition advanced a staged intent or was dropped as a replay or a move backward.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentTransition {
    /// The target phase was later than the intent's current phase, so the intent advanced to it.
    Advanced,
    /// The target was the current phase or earlier, or no such intent exists, so the phase stands.
    Ignored,
}

impl MetaStore {
    /// Stage `admission` under its authority, retaining its payload for the finalization path.
    ///
    /// The read, the ceiling check, and the insert share one write transaction, so two racing admissions
    /// of the same key never both take a slot. A key already staged for the same content is a
    /// [`Duplicate`](IntentStageOutcome::Duplicate); the same key for different content a
    /// [`Conflict`](IntentStageOutcome::Conflict). A new key is refused once its authority holds its record
    /// ceiling ([`RejectedOverRecordLimit`](IntentStageOutcome::RejectedOverRecordLimit)) or admitting it
    /// would cross its byte ceiling ([`RejectedOverByteLimit`](IntentStageOutcome::RejectedOverByteLimit)).
    /// The result also reports whether the authority's post-admission usage has crossed a soft threshold,
    /// so the caller sheds load ahead of the hard bound.
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

    /// Advance the intent under `intent_key` to `to`, returning whether it moved.
    ///
    /// The transition applies only when `to` is later than the intent's current phase, so a replayed or
    /// reordered event, or one naming an unknown intent, leaves the lifecycle unchanged. The per-authority
    /// counter is untouched: a settled intent still occupies its slot until the reaper prunes it.
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

    /// Read the intent staged under `intent_key`, or `None` when none is retained.
    ///
    /// # Errors
    /// Returns a store error when the row cannot be read or decoded.
    pub fn staged_intent(&self, intent_key: &str) -> Result<Option<StagedIntent>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(INGRESS_INTENT)?;
        Ok(table
            .get(intent_key)?
            .map(|value| serde_json::from_slice(value.value()))
            .transpose()?)
    }

    /// The retained records and bytes `authority` currently holds, the counter its ceilings bound.
    ///
    /// # Errors
    /// Returns a store error when the counter row cannot be read or decoded.
    pub fn staged_intent_usage(&self, authority: &str) -> Result<IntentUsage, MetaError> {
        let txn = self.db.begin_read()?;
        let counts = txn.open_table(INGRESS_INTENT_COUNT)?;
        read_usage(&counts, authority)
    }

    /// List up to `limit` intents still [`Pending`](IntentPhase::Pending), in admission order.
    ///
    /// The drain reads its work through this. The order index keys the pending set by the durable
    /// admission sequence, so a restart preserves interleaved authorities and admission order. A finalized intent has
    /// advanced out
    /// of the pending set, so it is skipped though its order entry lingers until it is pruned.
    ///
    /// # Errors
    /// Returns a store error when a table cannot be read or a record decoded.
    pub fn list_pending_intents(&self, limit: usize) -> Result<Vec<(String, StagedIntent)>, MetaError> {
        let txn = self.db.begin_read()?;
        let order = txn.open_table(INGRESS_INTENT_ORDER)?;
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

    /// How many intents the ledger currently retains across all authorities.
    ///
    /// # Errors
    /// Returns a store error when the table cannot be read.
    pub fn count_staged_intents(&self) -> Result<u64, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(INGRESS_INTENT)?;
        Ok(table.len()?)
    }

    /// Remove up to `limit` settled intents whose retention window has elapsed at `now`, returning how
    /// many were pruned. An [`Admitted`](IntentPhase::Admitted) intent the home DC finalized, or an
    /// [`Expired`](IntentPhase::Expired) one, is pruned once `retention_secs` have passed since its last
    /// transition. A [`Pending`](IntentPhase::Pending) intent is never pruned, since its write may still
    /// finalize, so a stalled home DC still sheds new load through admission rather than by dropping work
    /// in flight. Pruning releases the intent's order entry and returns its slot to the authority's
    /// counter.
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

/// Read `authority`'s retained-usage counter, defaulting to empty when it holds nothing.
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
