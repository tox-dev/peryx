//! The home datacenter's durable backlog of old-epoch operations awaiting reconciliation.
//!
//! After an authority transfers, the operations the old home durably recorded under the old epoch each
//! need one terminal disposition. This is that backlog: the reconciler enqueues an old-epoch operation
//! with the facts it derived, drains the backlog by stamping each entry's terminal outcome, and prunes a
//! settled entry once the replica and retention frontiers have passed it. The table is the durable state
//! that makes the drain restart-safe - a process that stops mid-backlog resumes from the entries still
//! pending, and an entry already settled is never re-run - so every operation reaches exactly one
//! outcome across restarts. Bounding the drain and prune batches keeps the reconciliation scan and the
//! retained backlog within their limits.
//!
//! Deriving the facts from the committed operation record and the current metadata, and turning a
//! settled outcome into a replay, live above this store in the reconciler and its
//! [`classify`](../../../peryx_ha_distributed/fn.classify.html) core.

use redb::{ReadableTable as _, ReadableTableMetadata as _};
use serde::{Deserialize, Serialize};

use super::{MetaError, MetaStore, RECONCILE_BACKLOG};

/// One durable old-epoch operation awaiting or holding its reconciliation outcome.
///
/// The three fact flags mirror what the reconciler derived; `outcome` is `None` while the entry is
/// pending and the terminal disposition code once the drain settles it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileEntry {
    pub source: String,
    /// The epoch the operation originally committed under, before the transfer.
    pub epoch: u64,
    pub serial: u64,
    pub durably_committed: bool,
    pub already_applied: bool,
    pub superseded: bool,
    /// The W3C traceparent the original operation carried, retained so a replay can continue its trace.
    pub traceparent: Option<String>,
    /// The stable terminal outcome code the drain stamped, or `None` while the entry is still pending.
    pub outcome: Option<String>,
    pub updated_at_unix: i64,
}

impl ReconcileEntry {
    /// Whether the entry still needs a terminal outcome.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        self.outcome.is_none()
    }
}

/// Whether enqueueing an old-epoch operation staged a new entry or found one already present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileEnqueue {
    /// A new entry was staged as pending.
    Enqueued,
    /// The key was already staged, so the first entry stands and its outcome is preserved.
    AlreadyPresent,
}

/// The identity and derived facts an old-epoch operation is enqueued with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewReconcileEntry<'a> {
    pub source: &'a str,
    pub epoch: u64,
    pub serial: u64,
    pub durably_committed: bool,
    pub already_applied: bool,
    pub superseded: bool,
    pub traceparent: Option<&'a str>,
}

impl NewReconcileEntry<'_> {
    /// The backlog key that binds one operation's identity, so a re-scan of the same operation after a
    /// restart resolves the existing entry rather than staging it twice.
    fn key(&self) -> String {
        format!("{}:{}:{}", self.source, self.epoch, self.serial)
    }
}

impl MetaStore {
    /// Stage `entry` as a pending reconciliation, or leave the existing entry untouched when its key is
    /// already staged.
    ///
    /// The read and the insert share one write transaction, so two racing enqueues of the same operation
    /// never both stage it, and an already-settled entry is never reset to pending - a re-scan after a
    /// restart is idempotent.
    ///
    /// # Errors
    /// Returns a store error when the row cannot be read, encoded, or committed.
    pub fn enqueue_reconcile(&self, entry: &NewReconcileEntry<'_>, now: i64) -> Result<ReconcileEnqueue, MetaError> {
        let key = entry.key();
        let txn = self.db.begin_write()?;
        let outcome;
        {
            let mut table = txn.open_table(RECONCILE_BACKLOG)?;
            if table.get(key.as_str())?.is_some() {
                outcome = ReconcileEnqueue::AlreadyPresent;
            } else {
                let record = ReconcileEntry {
                    source: entry.source.to_owned(),
                    epoch: entry.epoch,
                    serial: entry.serial,
                    durably_committed: entry.durably_committed,
                    already_applied: entry.already_applied,
                    superseded: entry.superseded,
                    traceparent: entry.traceparent.map(str::to_owned),
                    outcome: None,
                    updated_at_unix: now,
                };
                table.insert(key.as_str(), serde_json::to_vec(&record)?.as_slice())?;
                outcome = ReconcileEnqueue::Enqueued;
            }
        }
        txn.commit()?;
        Ok(outcome)
    }

    /// Read up to `limit` pending entries, each with its backlog key, in key order so a bounded drain
    /// makes deterministic forward progress a restart can resume.
    ///
    /// # Errors
    /// Returns a store error when a row cannot be read or decoded.
    pub fn pending_reconcile(&self, limit: usize) -> Result<Vec<(String, ReconcileEntry)>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(RECONCILE_BACKLOG)?;
        let mut pending = Vec::new();
        for entry in table.iter()? {
            if pending.len() >= limit {
                break;
            }
            let (key, value) = entry?;
            let record: ReconcileEntry = serde_json::from_slice(value.value())?;
            if record.is_pending() {
                pending.push((key.value().to_owned(), record));
            }
        }
        Ok(pending)
    }

    /// Stamp the entry under `key` with its terminal `outcome`, returning whether it moved from pending.
    ///
    /// Settling an entry that already holds an outcome, or a key that no longer exists, leaves the
    /// backlog unchanged, so a re-run of the drain never re-settles or double-counts an operation.
    ///
    /// # Errors
    /// Returns a store error when the row cannot be read, encoded, or committed.
    pub fn settle_reconcile(&self, key: &str, outcome: &str, now: i64) -> Result<bool, MetaError> {
        let txn = self.db.begin_write()?;
        let settled;
        {
            let mut table = txn.open_table(RECONCILE_BACKLOG)?;
            let existing = table
                .get(key)?
                .map(|value| serde_json::from_slice::<ReconcileEntry>(value.value()))
                .transpose()?;
            settled = match existing {
                Some(mut record) if record.is_pending() => {
                    record.outcome = Some(outcome.to_owned());
                    record.updated_at_unix = now;
                    table.insert(key, serde_json::to_vec(&record)?.as_slice())?;
                    true
                }
                _ => false,
            };
        }
        txn.commit()?;
        Ok(settled)
    }

    /// Read one entry, or `None` when its key is not staged.
    ///
    /// # Errors
    /// Returns a store error when the row cannot be read or decoded.
    pub fn reconcile_entry(&self, key: &str) -> Result<Option<ReconcileEntry>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(RECONCILE_BACKLOG)?;
        Ok(table
            .get(key)?
            .map(|value| serde_json::from_slice(value.value()))
            .transpose()?)
    }

    /// Remove up to `limit` settled entries whose serial both `replica_frontier` and `retention_frontier`
    /// have reached, returning how many were pruned.
    ///
    /// A pending entry is never pruned, and a settled entry a lagging replica or the audit window still
    /// trails is retained.
    ///
    /// # Errors
    /// Returns a store error when a row cannot be read or the delete cannot be committed.
    pub fn prune_reconcile(
        &self,
        replica_frontier: u64,
        retention_frontier: u64,
        limit: usize,
    ) -> Result<usize, MetaError> {
        let bound = replica_frontier.min(retention_frontier);
        let txn = self.db.begin_write()?;
        let pruned;
        {
            let mut table = txn.open_table(RECONCILE_BACKLOG)?;
            let mut doomed = Vec::new();
            for entry in table.iter()? {
                if doomed.len() >= limit {
                    break;
                }
                let (key, value) = entry?;
                let record: ReconcileEntry = serde_json::from_slice(value.value())?;
                if !record.is_pending() && record.serial <= bound {
                    doomed.push(key.value().to_owned());
                }
            }
            for key in &doomed {
                table.remove(key.as_str())?;
            }
            pruned = doomed.len();
        }
        txn.commit()?;
        Ok(pruned)
    }

    /// How many entries the backlog currently retains, pending or settled.
    ///
    /// # Errors
    /// Returns a store error when the table cannot be read.
    pub fn count_reconcile(&self) -> Result<u64, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(RECONCILE_BACKLOG)?;
        Ok(table.len()?)
    }
}

#[cfg(test)]
mod tests {
    use super::{NewReconcileEntry, ReconcileEnqueue};
    use crate::meta::MetaStore;

    const NOW: i64 = 1_800_000_000;

    fn store(dir: &tempfile::TempDir) -> MetaStore {
        MetaStore::open(dir.path().join("meta.redb")).unwrap()
    }

    fn entry(serial: u64) -> NewReconcileEntry<'static> {
        NewReconcileEntry {
            source: "east",
            epoch: 4,
            serial,
            durably_committed: true,
            already_applied: false,
            superseded: false,
            traceparent: None,
        }
    }

    #[test]
    fn test_enqueue_stages_a_pending_entry_once() {
        let dir = tempfile::tempdir().unwrap();
        let meta = store(&dir);

        assert_eq!(
            meta.enqueue_reconcile(&entry(1), NOW).unwrap(),
            ReconcileEnqueue::Enqueued
        );
        // A re-scan of the same operation resolves the existing entry rather than staging it twice.
        assert_eq!(
            meta.enqueue_reconcile(&entry(1), NOW + 10).unwrap(),
            ReconcileEnqueue::AlreadyPresent
        );
        assert_eq!(meta.count_reconcile().unwrap(), 1);
        let record = meta.reconcile_entry("east:4:1").unwrap().unwrap();
        assert!(record.is_pending());
        assert_eq!(record.updated_at_unix, NOW, "the first entry stands unchanged");
    }

    #[test]
    fn test_pending_reads_in_key_order_within_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let meta = store(&dir);
        for serial in [3, 1, 2] {
            meta.enqueue_reconcile(&entry(serial), NOW).unwrap();
        }

        let batch = meta.pending_reconcile(2).unwrap();
        let keys: Vec<_> = batch.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, ["east:4:1", "east:4:2"], "bounded and ordered");
    }

    #[test]
    fn test_settle_stamps_once_and_never_re_settles() {
        let dir = tempfile::tempdir().unwrap();
        let meta = store(&dir);
        meta.enqueue_reconcile(&entry(1), NOW).unwrap();

        assert!(meta.settle_reconcile("east:4:1", "replayable", NOW).unwrap());
        // A re-run of the drain never re-settles a terminal entry, and an unknown key is a no-op.
        assert!(!meta.settle_reconcile("east:4:1", "superseded", NOW + 5).unwrap());
        assert!(!meta.settle_reconcile("east:4:9", "failed", NOW).unwrap());

        let record = meta.reconcile_entry("east:4:1").unwrap().unwrap();
        assert_eq!(record.outcome.as_deref(), Some("replayable"));
        assert!(meta.pending_reconcile(10).unwrap().is_empty());
    }

    #[test]
    fn test_prune_releases_only_settled_entries_past_both_frontiers() {
        let dir = tempfile::tempdir().unwrap();
        let meta = store(&dir);
        for serial in [1, 2, 3] {
            meta.enqueue_reconcile(&entry(serial), NOW).unwrap();
        }
        meta.settle_reconcile("east:4:1", "replayable", NOW).unwrap();
        meta.settle_reconcile("east:4:2", "superseded", NOW).unwrap();
        // 3 stays pending.

        // The audit-retention frontier still trails serial 2, so only serial 1 releases.
        assert_eq!(meta.prune_reconcile(10, 1, 10).unwrap(), 1);
        assert!(meta.reconcile_entry("east:4:1").unwrap().is_none());
        // A replica frontier that trails holds everything else.
        assert_eq!(
            meta.prune_reconcile(1, 10, 10).unwrap(),
            0,
            "serial 2 still trailed by the replica"
        );
        // Both frontiers past serial 2 releases it; the pending serial 3 is never pruned.
        assert_eq!(meta.prune_reconcile(10, 10, 10).unwrap(), 1);
        assert!(meta.reconcile_entry("east:4:2").unwrap().is_none());
        assert!(meta.reconcile_entry("east:4:3").unwrap().unwrap().is_pending());
    }

    #[test]
    fn test_prune_bounds_its_batch() {
        let dir = tempfile::tempdir().unwrap();
        let meta = store(&dir);
        for serial in 1..=4 {
            meta.enqueue_reconcile(&entry(serial), NOW).unwrap();
            meta.settle_reconcile(&format!("east:4:{serial}"), "failed", NOW)
                .unwrap();
        }
        assert_eq!(
            meta.prune_reconcile(10, 10, 2).unwrap(),
            2,
            "one batch releases at most the limit"
        );
        assert_eq!(meta.count_reconcile().unwrap(), 2);
    }

    #[test]
    fn test_drain_resumes_across_a_restart_and_settles_each_op_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta.redb");
        {
            let meta = MetaStore::open(&path).unwrap();
            for serial in 1..=5 {
                meta.enqueue_reconcile(&entry(serial), NOW).unwrap();
            }
            // Drain a bounded first batch, then the process stops mid-backlog.
            for (key, _) in meta.pending_reconcile(2).unwrap() {
                assert!(meta.settle_reconcile(&key, "replayable", NOW).unwrap());
            }
        }

        // A fresh process reopens the durable backlog and resumes: the two settled entries are skipped,
        // and re-enqueueing an operation the crash may have re-scanned never resets its outcome.
        let meta = MetaStore::open(&path).unwrap();
        assert_eq!(
            meta.enqueue_reconcile(&entry(1), NOW + 100).unwrap(),
            ReconcileEnqueue::AlreadyPresent
        );
        let resumed = meta.pending_reconcile(10).unwrap();
        assert_eq!(resumed.len(), 3, "only the un-settled operations remain to reconcile");

        for (key, _) in resumed {
            assert!(meta.settle_reconcile(&key, "replayable", NOW + 100).unwrap());
        }
        assert!(
            meta.pending_reconcile(10).unwrap().is_empty(),
            "every operation reached a terminal outcome"
        );
        // Exactly once: none of the five re-settles.
        for serial in 1..=5 {
            assert!(
                !meta
                    .settle_reconcile(&format!("east:4:{serial}"), "failed", NOW + 200)
                    .unwrap()
            );
        }
    }
}
