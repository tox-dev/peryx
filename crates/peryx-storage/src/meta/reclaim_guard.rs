//! Deletion guards that coordinate single-node orphan collection with reference commits.
//!
//! The cache-purge collector proves a blob unreferenced, then unlinks its bytes. The proof and the
//! unlink are separate steps, so a writer that publishes a reference to the digest in between - a fresh
//! upload, or a dedup writer that finds the bytes already present and commits only the reference - would
//! be left pointing at content the collector is about to delete.
//!
//! A guard closes that window. The collector arms a guard for each candidate under a serial fence: the
//! guard is written only when the store serial still matches the one it scanned references at, so a
//! reference publication that raced the scan (which always advances the serial) fences the arm and forces
//! a re-scan. While a guard is armed, [`commit_driver_txn`](MetaStore::commit_driver_txn) rejects any
//! locally originated commit that references the digest with [`MetaError::BlobReclaiming`], so no
//! reference can be published to bytes that are mid-deletion. The collector disarms the guard once the
//! bytes are gone, and a fresh run clears any guard a crash stranded before selecting anew.

use redb::ReadableTable as _;

use super::{BLOB_RECLAIM_GUARD, MetaError, MetaStore, SERIAL, SERIAL_KEY};

impl MetaStore {
    /// Arm a deletion guard for every digest in `digests`, but only while the store serial still equals
    /// `expected_serial`.
    ///
    /// Returns `true` when the guards were armed, or `false` when the serial had advanced past
    /// `expected_serial` - the signal that a reference publication may have landed since the caller
    /// scanned, so it re-scans and retries rather than delete a blob a fresh commit now names. `now`
    /// timestamps each guard for operator visibility; re-arming a guarded digest refreshes it. The guard
    /// write does not itself advance the serial.
    ///
    /// # Errors
    /// Returns a store error if the transaction cannot be opened, read, written, or committed.
    pub fn arm_blob_reclaim_guards(&self, digests: &[&str], expected_serial: u64, now: i64) -> Result<bool, MetaError> {
        let txn = self.db.begin_write()?;
        let armed = {
            let serial = txn
                .open_table(SERIAL)?
                .get(SERIAL_KEY)?
                .map_or(0, |value| value.value());
            let fresh = serial == expected_serial;
            if fresh {
                let mut table = txn.open_table(BLOB_RECLAIM_GUARD)?;
                for &digest in digests {
                    table.insert(digest, now)?;
                }
            }
            fresh
        };
        txn.commit()?;
        Ok(armed)
    }

    /// Remove the deletion guard for `digest`, reporting whether one was armed. The collector calls this
    /// once the bytes are gone.
    ///
    /// # Errors
    /// Returns a store error if the transaction fails.
    pub fn disarm_blob_reclaim_guard(&self, digest: &str) -> Result<bool, MetaError> {
        let txn = self.db.begin_write()?;
        let removed = txn.open_table(BLOB_RECLAIM_GUARD)?.remove(digest)?.is_some();
        txn.commit()?;
        Ok(removed)
    }

    /// Whether a deletion guard is currently armed for `digest`.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn blob_reclaim_guarded(&self, digest: &str) -> Result<bool, MetaError> {
        let txn = self.db.begin_read()?;
        Ok(txn.open_table(BLOB_RECLAIM_GUARD)?.get(digest)?.is_some())
    }

    /// Drop every deletion guard, returning how many were cleared. A fresh collection run calls this so a
    /// guard a crashed prior run left armed cannot block references indefinitely.
    ///
    /// # Errors
    /// Returns a store error if the transaction fails.
    pub fn clear_blob_reclaim_guards(&self) -> Result<usize, MetaError> {
        let txn = self.db.begin_write()?;
        let cleared = {
            let mut table = txn.open_table(BLOB_RECLAIM_GUARD)?;
            let keys = table
                .iter()?
                .map(|entry| Ok(entry?.0.value().to_owned()))
                .collect::<Result<Vec<String>, MetaError>>()?;
            let count = keys.len();
            for key in keys {
                table.remove(key.as_str())?;
            }
            count
        };
        txn.commit()?;
        Ok(cleared)
    }
}
