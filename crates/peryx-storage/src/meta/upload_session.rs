//! Durable state for in-progress chunked blob uploads at the ingress DC.
//!
//! An OCI client opens an upload session, streams it in chunks, then finalizes it. Without durable
//! session state a process restart between chunks loses the session, forcing a full re-upload. This
//! persists each session's staged offset and target so a restart resumes from the last accepted chunk.
//!
//! The staged bytes themselves live durably in the blob store's per-session stage; this records only the
//! accounting a resume needs — how far the session has been staged and the repository it targets — keyed
//! by the session id. Reclaiming an idle session's record and its stage is a bounded pass the caller
//! drives.

use redb::ReadableTable as _;
use serde::{Deserialize, Serialize};

use super::{MetaError, MetaStore, UPLOAD_SESSION};

/// One in-progress upload session: how far it has been staged and the repository it targets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadRecord {
    /// Bytes durably staged so far; the offset a resumed chunk must continue from.
    pub offset: u64,
    /// The index the upload was opened against.
    pub index: String,
    /// The repository path the upload targets.
    pub name: String,
    pub updated_at_unix: i64,
}

impl MetaStore {
    /// Open a new upload session, staged at offset zero and targeting `index`/`name`.
    ///
    /// # Errors
    /// Returns a store error when the row cannot be encoded or committed.
    pub fn begin_upload(&self, session: &str, index: &str, name: &str, now: i64) -> Result<(), MetaError> {
        let record = UploadRecord {
            offset: 0,
            index: index.to_owned(),
            name: name.to_owned(),
            updated_at_unix: now,
        };
        let value = serde_json::to_vec(&record)?;
        let txn = self.db.begin_write()?;
        {
            txn.open_table(UPLOAD_SESSION)?.insert(session, value.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Record that `session` is now staged through `offset`. Returns `false` when no such session is
    /// open, so a chunk for an unknown or reclaimed session is rejected rather than resurrected.
    ///
    /// # Errors
    /// Returns a store error when the row cannot be read, encoded, or committed.
    pub fn advance_upload(&self, session: &str, offset: u64, now: i64) -> Result<bool, MetaError> {
        let txn = self.db.begin_write()?;
        let advanced;
        {
            let mut table = txn.open_table(UPLOAD_SESSION)?;
            let existing = table
                .get(session)?
                .map(|value| serde_json::from_slice::<UploadRecord>(value.value()))
                .transpose()?;
            match existing {
                Some(mut record) => {
                    record.offset = offset;
                    record.updated_at_unix = now;
                    table.insert(session, serde_json::to_vec(&record)?.as_slice())?;
                    advanced = true;
                }
                None => advanced = false,
            }
        }
        txn.commit()?;
        Ok(advanced)
    }

    /// Read the session's record, or `None` when it is unknown or reclaimed.
    ///
    /// # Errors
    /// Returns a store error when the row cannot be read or decoded.
    pub fn upload_record(&self, session: &str) -> Result<Option<UploadRecord>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(UPLOAD_SESSION)?;
        Ok(table
            .get(session)?
            .map(|value| serde_json::from_slice(value.value()))
            .transpose()?)
    }

    /// Close a session, returning whether one was open. Called when an upload finalizes or is cancelled.
    ///
    /// # Errors
    /// Returns a store error when the delete cannot be committed.
    pub fn remove_upload(&self, session: &str) -> Result<bool, MetaError> {
        let txn = self.db.begin_write()?;
        let existed;
        {
            let mut table = txn.open_table(UPLOAD_SESSION)?;
            existed = table.remove(session)?.is_some();
        }
        txn.commit()?;
        Ok(existed)
    }

    /// Remove up to `limit` sessions untouched at or before `cutoff`, returning their ids so the caller
    /// discards each one's staged bytes.
    ///
    /// # Errors
    /// Returns a store error when a row cannot be read or the delete cannot be committed.
    pub fn reclaim_uploads(&self, cutoff: i64, limit: usize) -> Result<Vec<String>, MetaError> {
        let txn = self.db.begin_write()?;
        let expired;
        {
            let mut table = txn.open_table(UPLOAD_SESSION)?;
            let mut doomed = Vec::new();
            for entry in table.iter()? {
                if doomed.len() >= limit {
                    break;
                }
                let (key, value) = entry?;
                let record: UploadRecord = serde_json::from_slice(value.value())?;
                if record.updated_at_unix <= cutoff {
                    doomed.push(key.value().to_owned());
                }
            }
            for session in &doomed {
                table.remove(session.as_str())?;
            }
            expired = doomed;
        }
        txn.commit()?;
        Ok(expired)
    }
}
