//! Idempotent apply for the additive analytics aggregates a producer node replicates to a replica.
//!
//! A producer folds request-path downloads into low-cardinality daily buckets and ships them as an
//! [`AnalyticsBatch`]: a set of additive `(dimension, downloads, bytes)` rows stamped with the
//! producer interval that emitted them. A replica applies each interval exactly once. The transfer
//! that carries a batch between nodes is a separate concern (issue #506); this boundary is only the
//! schema and the apply, so it stays verifiable without a live peer.
//!
//! Idempotence is the whole point: a duplicate, reordered, delayed, or retried batch must converge to
//! the same accepted totals a single delivery would. [`ApplyState`] keys deduplication on the
//! [`IntervalId`] `(producer, epoch, sequence)` triple, so a replay is recognized and dropped without
//! disturbing a total, and a producer restart under a fresh [`AuthorityEpoch`] is a distinct identity
//! that applies rather than a lost or double-counted interval.
//!
//! Every untrusted input is bounded and every recovery fails closed. A batch wider than
//! [`ApplyLimits::max_rows_per_batch`] is rejected before it folds, deduplication state cannot grow
//! past [`ApplyLimits::max_retained_intervals`] without [`ApplyState::compact`] first releasing the
//! intervals a durable [`Frontier`] covers, and a snapshot written under an unrecognized schema is
//! refused rather than rebuilt from zero, because silently resetting accepted totals or the replay
//! set would double-count the next replay.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

pub use peryx_ha::{
    AggregateDelta, AggregateKey, AggregateRow, AnalyticsBatch, AuthorityEpoch, IntervalId, ProducerId,
};

/// The snapshot schema this build writes and is willing to restore.
pub const APPLY_STATE_SCHEMA: u32 = 2;

/// The default apply bounds: a batch is metadata-sized, and retained replay keys stay bounded so a
/// stalled compaction frontier cannot grow the deduplication set without limit.
pub const DEFAULT_APPLY_LIMITS: ApplyLimits = ApplyLimits {
    max_rows_per_batch: 16_384,
    max_retained_intervals: 65_536,
};

/// The bounds a replica enforces on untrusted apply input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApplyLimits {
    /// The most rows one batch may carry before it is rejected unfolded.
    pub max_rows_per_batch: usize,
    /// The most interval keys the replay set may retain before a fresh interval is refused until
    /// compaction releases room.
    pub max_retained_intervals: usize,
}

impl Default for ApplyLimits {
    fn default() -> Self {
        DEFAULT_APPLY_LIMITS
    }
}

/// Whether a batch folded into the accepted totals or was recognized as an already-applied replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// The interval was new; its rows were folded into the accepted totals.
    Applied,
    /// The interval was already accepted; the totals were left unchanged.
    Duplicate,
}

/// A batch a replica refuses because it breaches a bound. Both variants fail closed, leaving the
/// accepted totals and replay set untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ApplyError {
    #[error("analytics batch carries {actual} rows, over the {limit} row apply limit")]
    BatchTooLarge { limit: usize, actual: usize },
    #[error("replay set holds {limit} intervals; compact past the durable frontier before applying more")]
    RetentionFull { limit: usize },
}

/// A snapshot that cannot be trusted to preserve replay protection, so restore refuses it rather than
/// silently rebuilding accepted totals from zero.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("analytics apply snapshot is malformed: {0}")]
    Malformed(#[source] serde_json::Error),
    #[error("analytics apply snapshot schema {found} is not the {expected} this build restores")]
    UnsupportedSchema { expected: u32, found: u32 },
}

/// The highest interval sequence each `(producer, epoch)` has durably passed everywhere.
///
/// Replay protection must outlast the producer, this replica, and the backup. Compaction may release
/// only the intervals this combined frontier covers, because a producer never resends below the
/// sequence it has been acknowledged for, so those keys can no longer be replayed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Frontier {
    acknowledged: BTreeMap<ProducerId, BTreeMap<AuthorityEpoch, u64>>,
}

impl Frontier {
    /// Record that `producer`'s `epoch` is durable through `sequence`, keeping the highest sequence
    /// seen so an out-of-order acknowledgement never retracts coverage.
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

    /// Every `(producer, epoch, sequence)` the frontier holds, flattened for a snapshot to persist. A
    /// nested integer-keyed map does not round-trip through JSON, so the durable form is this tuple list.
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

/// An accepted aggregate's dimension stamped with the producer that reported it. Folding keeps the
/// producer in the key so a completeness query can restrict its totals to the expected producer set,
/// leaving an unexpected or decommissioned producer's rows out of the reported sums instead of letting
/// them inflate a total the verdict never consulted them for.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct AcceptedKey {
    producer: ProducerId,
    dimension: AggregateKey,
}

/// The persisted serde shape of an [`ApplyState`]: a schema tag guarding the accepted totals and the
/// replay set, so a format change is a deliberate migration rather than a silent misread.
#[derive(Debug, Serialize, Deserialize)]
struct ApplyStateSnapshot {
    schema: u32,
    totals: Vec<AcceptedSnapshotRow>,
    applied: Vec<IntervalId>,
}

/// One accepted total in a snapshot: the producer that reported the dimension, the dimension, and its
/// converged delta.
#[derive(Debug, Serialize, Deserialize)]
struct AcceptedSnapshotRow {
    producer: ProducerId,
    key: AggregateKey,
    delta: AggregateDelta,
}

/// A replica's converged view: the accepted additive totals plus the replay set that keeps applying a
/// batch idempotent.
#[derive(Debug, Clone)]
pub struct ApplyState {
    totals: BTreeMap<AcceptedKey, AggregateDelta>,
    applied: BTreeSet<IntervalId>,
    limits: ApplyLimits,
}

impl ApplyState {
    /// An empty state bounded by `limits`.
    #[must_use]
    pub const fn new(limits: ApplyLimits) -> Self {
        Self {
            totals: BTreeMap::new(),
            applied: BTreeSet::new(),
            limits,
        }
    }

    /// Apply `batch` once. A batch whose interval was already accepted is a
    /// [`ApplyOutcome::Duplicate`] that changes nothing; a new interval folds its rows into the
    /// accepted totals and joins the replay set.
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

    /// The accepted total for `key` across every producer that reported it, or a zero delta for a
    /// dimension nothing has folded into.
    #[must_use]
    pub fn total(&self, key: &AggregateKey) -> AggregateDelta {
        self.totals
            .iter()
            .filter(|(stored, _)| &stored.dimension == key)
            .fold(AggregateDelta::default(), |sum, (_, delta)| sum.saturating_add(*delta))
    }

    /// The number of interval keys the replay set currently retains.
    #[must_use]
    pub fn retained_intervals(&self) -> usize {
        self.applied.len()
    }

    /// Drop the replay keys `frontier` covers, bounding deduplication memory once an interval can no
    /// longer be replayed. Accepted totals are never touched, so releasing a key preserves the sum.
    pub fn compact(&mut self, frontier: &Frontier) {
        self.applied.retain(|interval| !frontier.covers(interval));
    }

    /// Serialize the accepted totals and replay set to their JSON snapshot form.
    ///
    /// # Panics
    /// Panics only if serializing to JSON fails, which the field types make unreachable.
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

    /// Restore a state from its snapshot under `limits`, preserving both the accepted totals and the
    /// replay protection.
    ///
    /// # Errors
    /// Returns [`SnapshotError::Malformed`] for unparseable bytes and [`SnapshotError::UnsupportedSchema`]
    /// for a schema this build does not restore. Restore fails closed rather than rebuild from zero,
    /// because a reset replay set would double-count the next replay.
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

/// The receiving side of analytics replication on a replica.
///
/// It holds the converged [`ApplyState`], the per-producer sealed-day cursor a pull resumes from, the
/// per-producer accepted frontier a completeness query reads, and the durability [`Frontier`] compaction
/// releases replay keys past.
///
/// The cursor is the highest sealed-day sequence this replica has accepted from each producer, so a pull
/// asks only for days beyond it and a re-pull of an accepted day is a recognized duplicate. Compaction
/// runs off a separate [`acknowledge`](Self::acknowledge) signal: a replay key is released only once it
/// is durable everywhere, never merely because this replica applied it, since a producer may still resend
/// it to a peer that has not caught up.
///
/// The accepted frontier is the highest `(epoch, sequence)` interval this replica has folded from each
/// producer. Unlike the cursor it keeps the epoch, so a completeness query can tell a producer that
/// restarted under a fresh generation apart from one still on its original one, and unlike the durability
/// frontier it survives compaction, since releasing a replay key never lowers the position already
/// accepted.
#[derive(Debug, Clone)]
pub struct AnalyticsReceiver {
    state: ApplyState,
    cursors: BTreeMap<ProducerId, i64>,
    accepted: BTreeMap<ProducerId, (AuthorityEpoch, u64)>,
    frontier: Frontier,
}

/// The persisted serde shape of an [`AnalyticsReceiver`], guarded by a schema tag so a format change is
/// a deliberate migration rather than a silent misread.
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
    /// An empty receiver bounded by `limits`.
    #[must_use]
    pub fn new(limits: ApplyLimits) -> Self {
        Self {
            state: ApplyState::new(limits),
            cursors: BTreeMap::new(),
            accepted: BTreeMap::new(),
            frontier: Frontier::default(),
        }
    }

    /// The last sealed day accepted from `producer`, or `-1` before any, so a pull requests every day
    /// from day zero onward.
    #[must_use]
    pub fn after_day(&self, producer: &ProducerId) -> i64 {
        self.cursors.get(producer).copied().unwrap_or(-1)
    }

    /// The highest sealed day accepted from any producer, or `-1` before any. A replica pulling one
    /// upstream resumes from this, since that upstream's batches are the only ones advancing it.
    #[must_use]
    pub fn resume_day(&self) -> i64 {
        self.cursors.values().copied().max().unwrap_or(-1)
    }

    /// Apply one pulled batch. A new interval folds its rows and advances the producer's cursor to its
    /// day; an already-accepted interval is a [`Duplicate`](ApplyOutcome::Duplicate) that changes
    /// nothing, so a reordered, delayed, or retried delivery converges to the same accepted totals.
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

    /// The highest `(epoch, sequence)` interval accepted from `producer`, or `None` before any. A
    /// completeness query reads it as the producer's accepted frontier: how far its stream this replica
    /// has folded, epoch and all.
    #[must_use]
    pub fn accepted_frontier(&self, producer: &ProducerId) -> Option<(AuthorityEpoch, u64)> {
        self.accepted.get(producer).copied()
    }

    /// The accepted aggregate dimensions and their converged totals, each tagged with the producer that
    /// reported it, for a completeness query to fold over a day range and repository scope while
    /// excluding producers outside its expected set.
    pub(crate) fn accepted_rows(&self) -> impl Iterator<Item = (&ProducerId, &AggregateKey, &AggregateDelta)> {
        self.state
            .totals
            .iter()
            .map(|(key, delta)| (&key.producer, &key.dimension, delta))
    }

    /// The accepted total for `key`.
    #[must_use]
    pub fn total(&self, key: &AggregateKey) -> AggregateDelta {
        self.state.total(key)
    }

    /// The number of replay keys the receiver retains.
    #[must_use]
    pub fn retained_intervals(&self) -> usize {
        self.state.retained_intervals()
    }

    /// Record that `(producer, epoch)` is durable everywhere through `sequence`, so a later
    /// [`compact`](Self::compact) may release the replay keys it now covers.
    pub fn acknowledge(&mut self, producer: ProducerId, epoch: AuthorityEpoch, sequence: u64) {
        self.frontier.acknowledge(producer, epoch, sequence);
    }

    /// Release every replay key the durability frontier now covers, leaving the accepted totals intact.
    pub fn compact(&mut self) {
        self.state.compact(&self.frontier);
    }

    /// Serialize the receiver's converged state, per-producer cursors, and durability frontier.
    ///
    /// # Panics
    /// Panics only if serializing to JSON fails, which the field types make unreachable.
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

    /// Restore a receiver from its snapshot under `limits`, preserving the accepted totals, the cursors,
    /// and the replay protection.
    ///
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
