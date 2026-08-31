//! A reclamation pass reads a bounded page of blob digests and a bounded page of tombstones, so each
//! phase records where its scan stopped and the next pass resumes there instead of restarting at the
//! first row and reselecting the same slice forever.

use super::{MetaError, MetaStore, RECLAMATION_CURSOR, open_optional_table};

/// The two scans a pass advances independently: selection walks blob digests, finalization walks
/// tombstones, and neither may drag the other back to its own position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReclamationPhase {
    Selection,
    Finalize,
}

impl ReclamationPhase {
    const fn key(self) -> &'static str {
        match self {
            Self::Selection => "selection",
            Self::Finalize => "finalize",
        }
    }
}

impl MetaStore {
    /// Returns `None` when the phase's next pass starts at the first row.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn reclamation_cursor(&self, phase: ReclamationPhase) -> Result<Option<String>, MetaError> {
        let txn = self.db.begin_read()?;
        let Some(table) = open_optional_table(&txn, RECLAMATION_CURSOR)? else {
            return Ok(None);
        };
        Ok(table.get(phase.key())?.map(|value| value.value().to_owned()))
    }

    /// `None` wraps the phase back to the first row, which is how a completed scan starts over.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    pub fn set_reclamation_cursor(&self, phase: ReclamationPhase, cursor: Option<&str>) -> Result<(), MetaError> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(RECLAMATION_CURSOR)?;
            match cursor {
                Some(cursor) => {
                    table.insert(phase.key(), cursor)?;
                }
                None => {
                    table.remove(phase.key())?;
                }
            }
        }
        txn.commit()?;
        Ok(())
    }
}
