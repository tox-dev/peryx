//! Replicas apply additive analytics once per `(producer, epoch, sequence)` interval. A new epoch
//! distinguishes intervals emitted after a producer restart.
//!
//! Apply bounds leave state unchanged on failure. Compaction releases replay keys after a durable
//! frontier covers them. Restore rejects unknown schemas because resetting totals or replay protection
//! would double-count later replays.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

pub use peryx_ha::{
    AggregateDelta, AggregateKey, AggregateRow, AnalyticsBatch, AuthorityEpoch, IntervalId, ProducerId,
};

pub const APPLY_STATE_SCHEMA: u32 = 2;

pub const DEFAULT_APPLY_LIMITS: ApplyLimits = ApplyLimits {
    max_rows_per_batch: 16_384,
    max_retained_intervals: 65_536,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApplyLimits {
    /// Rejects larger batches before folding any row.
    pub max_rows_per_batch: usize,
    /// At this count, apply rejects new intervals until compaction releases room.
    pub max_retained_intervals: usize,
}

impl Default for ApplyLimits {
    fn default() -> Self {
        DEFAULT_APPLY_LIMITS
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied,
    Duplicate,
}

/// Apply failures leave accepted totals and replay keys unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ApplyError {
    #[error("analytics batch carries {actual} rows, over the {limit} row apply limit")]
    BatchTooLarge { limit: usize, actual: usize },
    #[error("replay set holds {limit} intervals; compact past the durable frontier before applying more")]
    RetentionFull { limit: usize },
}

/// Restore refuses snapshots that cannot preserve replay protection.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("analytics apply snapshot is malformed: {0}")]
    Malformed(#[source] serde_json::Error),
    #[error("analytics apply snapshot schema {found} is not the {expected} this build restores")]
    UnsupportedSchema { expected: u32, found: u32 },
}

/// The highest interval sequence with durability across all nodes for each `(producer, epoch)`.
/// Compaction may not release uncovered intervals because producers can resend them.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Frontier {
    acknowledged: BTreeMap<ProducerId, BTreeMap<AuthorityEpoch, u64>>,
}

impl Frontier {
    /// Keeps the highest sequence so an out-of-order acknowledgement cannot retract coverage.
    pub fn acknowledge(&mut self, producer: ProducerId, epoch: AuthorityEpoch, sequence: u64) {
        let slot = self.acknowledged.entry(producer).or_default().entry(epoch).or_default();
        *slot = (*slot).max(sequence);
    }

    fn covers(&self, interval: &IntervalId) -> bool {
        self.acknowledged
            .get(&interval.producer)
            .and_then(|epochs| epochs.get(&interval.epoch))
            .is_some_and(|&acknowledged| interval.sequence <= acknowledged)
    }

    /// Flattens the frontier because JSON cannot round-trip the nested integer-keyed map.
    #[must_use]
    pub fn acknowledgements(&self) -> Vec<(ProducerId, AuthorityEpoch, u64)> {
        self.acknowledged
            .iter()
            .flat_map(|(producer, epochs)| {
                epochs
                    .iter()
                    .map(move |(&epoch, &sequence)| (producer.clone(), epoch, sequence))
            })
            .collect()
    }
}

/// Producer identity lets completeness totals exclude producers outside the topology.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct AcceptedKey {
    producer: ProducerId,
    dimension: AggregateKey,
}

/// The schema tag prevents format changes from corrupting totals or replay protection.
#[derive(Debug, Serialize, Deserialize)]
struct ApplyStateSnapshot {
    schema: u32,
    totals: Vec<AcceptedSnapshotRow>,
    applied: Vec<IntervalId>,
}

#[derive(Debug, Serialize, Deserialize)]
struct AcceptedSnapshotRow {
    producer: ProducerId,
    key: AggregateKey,
    delta: AggregateDelta,
}

/// Accepted additive totals and the replay keys that make apply idempotent.
#[derive(Debug, Clone)]
pub struct ApplyState {
    totals: BTreeMap<AcceptedKey, AggregateDelta>,
    applied: BTreeSet<IntervalId>,
    limits: ApplyLimits,
}

impl ApplyState {
    #[must_use]
    pub const fn new(limits: ApplyLimits) -> Self {
        Self {
            totals: BTreeMap::new(),
            applied: BTreeSet::new(),
            limits,
        }
    }

    /// Applies each interval once; a duplicate leaves state unchanged.
    ///
    /// # Errors
    /// Returns [`ApplyError::BatchTooLarge`] for a batch over the row limit, or
    /// [`ApplyError::RetentionFull`] when a new interval would grow the replay set past its bound
    /// before [`ApplyState::compact`] has released room. Both leave the state unchanged.
    pub fn apply(&mut self, batch: &AnalyticsBatch) -> Result<ApplyOutcome, ApplyError> {
        if batch.rows.len() > self.limits.max_rows_per_batch {
            return Err(ApplyError::BatchTooLarge {
                limit: self.limits.max_rows_per_batch,
                actual: batch.rows.len(),
            });
        }
        if self.applied.contains(&batch.interval) {
            return Ok(ApplyOutcome::Duplicate);
        }
        if self.applied.len() >= self.limits.max_retained_intervals {
            return Err(ApplyError::RetentionFull {
                limit: self.limits.max_retained_intervals,
            });
        }
        for row in &batch.rows {
            let key = AcceptedKey {
                producer: batch.interval.producer.clone(),
                dimension: row.key.clone(),
            };
            let total = self.totals.entry(key).or_default();
            *total = total.saturating_add(row.delta);
        }
        self.applied.insert(batch.interval.clone());
        Ok(ApplyOutcome::Applied)
    }

    /// Returns the total across all producers, or zero when none reported `key`.
    #[must_use]
    pub fn total(&self, key: &AggregateKey) -> AggregateDelta {
        self.totals
            .iter()
            .filter(|(stored, _)| &stored.dimension == key)
            .fold(AggregateDelta::default(), |sum, (_, delta)| sum.saturating_add(*delta))
    }

    #[must_use]
    pub fn retained_intervals(&self) -> usize {
        self.applied.len()
    }

    /// Drops covered replay keys without changing accepted totals.
    pub fn compact(&mut self, frontier: &Frontier) {
        self.applied.retain(|interval| !frontier.covers(interval));
    }

    /// # Panics
    /// Panics if JSON serialization fails; the field types make this unreachable.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let snapshot = ApplyStateSnapshot {
            schema: APPLY_STATE_SCHEMA,
            totals: self
                .totals
                .iter()
                .map(|(key, delta)| AcceptedSnapshotRow {
                    producer: key.producer.clone(),
                    key: key.dimension.clone(),
                    delta: *delta,
                })
                .collect(),
            applied: self.applied.iter().cloned().collect(),
        };
        serde_json::to_vec(&snapshot).expect("an apply-state snapshot always serializes to JSON")
    }

    /// # Errors
    /// Returns [`SnapshotError::Malformed`] for unparseable bytes and [`SnapshotError::UnsupportedSchema`]
    /// for a schema this build does not restore. Restore fails closed because resetting the replay set
    /// would double-count the next replay.
    pub fn restore(bytes: &[u8], limits: ApplyLimits) -> Result<Self, SnapshotError> {
        let snapshot: ApplyStateSnapshot = serde_json::from_slice(bytes).map_err(SnapshotError::Malformed)?;
        if snapshot.schema != APPLY_STATE_SCHEMA {
            return Err(SnapshotError::UnsupportedSchema {
                expected: APPLY_STATE_SCHEMA,
                found: snapshot.schema,
            });
        }
        Ok(Self {
            totals: snapshot
                .totals
                .into_iter()
                .map(|row| {
                    (
                        AcceptedKey {
                            producer: row.producer,
                            dimension: row.key,
                        },
                        row.delta,
                    )
                })
                .collect(),
            applied: snapshot.applied.into_iter().collect(),
            limits,
        })
    }
}

/// Separates accepted progress from durability so compaction cannot release replay keys early.
#[derive(Debug, Clone)]
pub struct AnalyticsReceiver {
    state: ApplyState,
    cursors: BTreeMap<ProducerId, i64>,
    accepted: BTreeMap<ProducerId, (AuthorityEpoch, u64)>,
    frontier: Frontier,
}

/// The schema tag prevents format changes from corrupting receiver state.
#[derive(Serialize, Deserialize)]
struct ReceiverSnapshot {
    schema: u32,
    state: Vec<u8>,
    cursors: Vec<(ProducerId, i64)>,
    accepted: Vec<FrontierAck>,
    frontier: Vec<FrontierAck>,
}

#[derive(Serialize, Deserialize)]
struct FrontierAck {
    producer: ProducerId,
    epoch: AuthorityEpoch,
    sequence: u64,
}

impl AnalyticsReceiver {
    #[must_use]
    pub fn new(limits: ApplyLimits) -> Self {
        Self {
            state: ApplyState::new(limits),
            cursors: BTreeMap::new(),
            accepted: BTreeMap::new(),
            frontier: Frontier::default(),
        }
    }

    /// Returns `-1` before the producer's first accepted day.
    #[must_use]
    pub fn after_day(&self, producer: &ProducerId) -> i64 {
        self.cursors.get(producer).copied().unwrap_or(-1)
    }

    /// Returns the highest accepted day, or `-1` before any. Single-upstream pulls resume from this value.
    #[must_use]
    pub fn resume_day(&self) -> i64 {
        self.cursors.values().copied().max().unwrap_or(-1)
    }

    /// Advances cursor and accepted positions for new intervals; duplicates change nothing.
    ///
    /// # Errors
    /// Returns [`ApplyError`] when the batch breaches a bound, leaving the receiver unchanged.
    pub fn apply(&mut self, batch: &AnalyticsBatch) -> Result<ApplyOutcome, ApplyError> {
        let outcome = self.state.apply(batch)?;
        if outcome == ApplyOutcome::Applied {
            let day = i64::try_from(batch.interval.sequence).unwrap_or(i64::MAX);
            let cursor = self.cursors.entry(batch.interval.producer.clone()).or_insert(-1);
            *cursor = (*cursor).max(day);
            let position = (batch.interval.epoch, batch.interval.sequence);
            let accepted = self.accepted.entry(batch.interval.producer.clone()).or_insert(position);
            *accepted = (*accepted).max(position);
        }
        Ok(outcome)
    }

    /// Includes the epoch so completeness can distinguish producer generations.
    #[must_use]
    pub fn accepted_frontier(&self, producer: &ProducerId) -> Option<(AuthorityEpoch, u64)> {
        self.accepted.get(producer).copied()
    }

    pub(crate) fn accepted_rows(&self) -> impl Iterator<Item = (&ProducerId, &AggregateKey, &AggregateDelta)> {
        self.state
            .totals
            .iter()
            .map(|(key, delta)| (&key.producer, &key.dimension, delta))
    }

    #[must_use]
    pub fn total(&self, key: &AggregateKey) -> AggregateDelta {
        self.state.total(key)
    }

    #[must_use]
    pub fn retained_intervals(&self) -> usize {
        self.state.retained_intervals()
    }

    /// Marks a sequence durable everywhere, allowing later compaction to release covered replay keys.
    pub fn acknowledge(&mut self, producer: ProducerId, epoch: AuthorityEpoch, sequence: u64) {
        self.frontier.acknowledge(producer, epoch, sequence);
    }

    /// Releases covered replay keys without changing accepted totals.
    pub fn compact(&mut self) {
        self.state.compact(&self.frontier);
    }

    /// # Panics
    /// Panics if JSON serialization fails; the field types make this unreachable.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let snapshot = ReceiverSnapshot {
            schema: APPLY_STATE_SCHEMA,
            state: self.state.encode(),
            cursors: self
                .cursors
                .iter()
                .map(|(producer, &day)| (producer.clone(), day))
                .collect(),
            accepted: self
                .accepted
                .iter()
                .map(|(producer, &(epoch, sequence))| FrontierAck {
                    producer: producer.clone(),
                    epoch,
                    sequence,
                })
                .collect(),
            frontier: self
                .frontier
                .acknowledgements()
                .into_iter()
                .map(|(producer, epoch, sequence)| FrontierAck {
                    producer,
                    epoch,
                    sequence,
                })
                .collect(),
        };
        serde_json::to_vec(&snapshot).expect("a receiver snapshot always serializes to JSON")
    }

    /// # Errors
    /// Returns [`SnapshotError::Malformed`] for unparseable bytes and [`SnapshotError::UnsupportedSchema`]
    /// for a schema this build does not restore.
    pub fn restore(bytes: &[u8], limits: ApplyLimits) -> Result<Self, SnapshotError> {
        let snapshot: ReceiverSnapshot = serde_json::from_slice(bytes).map_err(SnapshotError::Malformed)?;
        if snapshot.schema != APPLY_STATE_SCHEMA {
            return Err(SnapshotError::UnsupportedSchema {
                expected: APPLY_STATE_SCHEMA,
                found: snapshot.schema,
            });
        }
        let mut frontier = Frontier::default();
        for ack in snapshot.frontier {
            frontier.acknowledge(ack.producer, ack.epoch, ack.sequence);
        }
        Ok(Self {
            state: ApplyState::restore(&snapshot.state, limits)?,
            cursors: snapshot.cursors.into_iter().collect(),
            accepted: snapshot
                .accepted
                .into_iter()
                .map(|ack| (ack.producer, (ack.epoch, ack.sequence)))
                .collect(),
            frontier,
        })
    }
}
