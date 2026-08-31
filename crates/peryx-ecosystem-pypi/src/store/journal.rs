use std::ops::ControlFlow;

use serde::{Deserialize, Serialize};

use peryx_storage::meta::{MetaError, MetaStore};

use crate::{ChangelogEntry, ChangelogPage, ChangelogPageError};

const PYPI_OP_TAG: &str = "pypi-op";

/// One recorded mutation in the [`MetaStore`] journal: the append-only changelog that makes peryx
/// an origin others can replicate from. `serial` orders entries; the rest names what changed.
///
/// The neutral serial counter lives in the store, so a `PyPI` publish builds this entry with a
/// placeholder `serial` and lets [`commit_driver_txn`] allocate the authoritative one - see
/// [`publish_file_if`](super::publish_file_if).
///
/// The journal is shared - peryx's own core operations and every other ecosystem driver append to the
/// same log - so an entry names its vocabulary with the `pypi-op` tag and a reader takes only the
/// records carrying it. Identifying its own records rather than recognizing foreign ones is what makes
/// the classification total: a driver peryx has never heard of writes records this reader passes over
/// without knowing anything about them.
///
/// [`MetaStore`]: peryx_storage::meta::MetaStore
/// [`commit_driver_txn`]: peryx_storage::meta::MetaStore::commit_driver_txn
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "pypi-op", rename = "journal-entry")]
pub struct JournalEntry {
    pub serial: u64,
    #[serde(default)]
    pub submitted_at_unix: i64,
    pub action: String,
    pub project: String,
    pub version: Option<String>,
    pub filename: Option<String>,
}

/// Decoded `PyPI` journal entries and the head serial from one storage snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalSnapshot {
    pub current_serial: u64,
    pub entries: Vec<JournalEntry>,
}

/// Why a journal snapshot cannot become a Warehouse changelog page.
#[derive(Debug, thiserror::Error)]
pub enum ChangelogReadError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error(transparent)]
    InvalidPage(#[from] ChangelogPageError),
}

/// # Errors
/// Returns a storage or page-validation error when the snapshot cannot be served safely.
pub fn read_changelog_page(meta: &MetaStore, after: i64, limit: usize) -> Result<ChangelogPage, ChangelogReadError> {
    let snapshot = read_journal_entries(meta, u64::try_from(after).unwrap_or(0), limit)?;
    let entries = snapshot
        .entries
        .into_iter()
        .map(|entry| ChangelogEntry {
            project: entry.project,
            version: entry.version,
            timestamp: entry.submitted_at_unix,
            action: warehouse_action(&entry.action, entry.filename.as_deref()),
            serial: entry.serial,
        })
        .collect();
    Ok(ChangelogPage::new(after, snapshot.current_serial, entries)?)
}

/// Read at most `limit` `PyPI` entries after `after`, with the head serial from the same snapshot.
///
/// Storage owns the serial, so this replaces the serialized placeholder with the record key.
///
/// `limit` bounds the entries returned rather than the records examined, so the walk runs past the
/// records other vocabularies wrote instead of spending a page on them. A window that bounded records
/// instead could hold nothing but foreign entries, and a Warehouse client - which resumes from the
/// highest serial a page returned, having no other cursor - would ask for that same window forever.
///
/// # Errors
/// Returns a store error if the snapshot cannot be read or a `PyPI` entry cannot be decoded.
pub fn read_journal_entries(meta: &MetaStore, after: u64, limit: usize) -> Result<JournalSnapshot, MetaError> {
    let mut entries = Vec::new();
    let mut failure = None;
    let current_serial = meta.visit_journal_page(after, usize::MAX, |record| {
        if entries.len() == limit {
            return ControlFlow::Break(());
        }
        match JournalEntry::decode(&record.payload) {
            Ok(None) => ControlFlow::Continue(()),
            Ok(Some(mut entry)) => {
                entry.serial = record.serial;
                entries.push(entry);
                ControlFlow::Continue(())
            }
            Err(error) => {
                failure = Some(error);
                ControlFlow::Break(())
            }
        }
    })?;
    if let Some(error) = failure {
        return Err(error.into());
    }
    Ok(JournalSnapshot {
        current_serial,
        entries,
    })
}

impl JournalEntry {
    /// Returns `None` when another vocabulary on the shared journal wrote the payload.
    ///
    /// The two failure modes are not symmetric, so the tag decides between them rather than the decode
    /// outcome: passing over a foreign record is correct, while passing over a `PyPI` record this build
    /// cannot read would drop a mutation a mirror needs and never mention it.
    ///
    /// # Errors
    /// Returns a decode error when the payload is not the tagged object every vocabulary writes, or
    /// when it claims the `PyPI` tag and still does not describe an entry.
    fn decode(payload: &[u8]) -> Result<Option<Self>, serde_json::Error> {
        let fields = serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(payload)?;
        if !fields.contains_key(PYPI_OP_TAG) {
            return Ok(None);
        }
        serde_json::from_value(serde_json::Value::Object(fields)).map(Some)
    }
}

fn warehouse_action(action: &str, filename: Option<&str>) -> String {
    let action = match action {
        "add-file" | "promote" => "add file",
        "delete-file" => "remove file",
        action => action,
    };
    filename.map_or_else(|| action.to_owned(), |filename| format!("{action} {filename}"))
}

#[cfg(test)]
#[path = "../../tests/unit/store/journal/tests.rs"]
mod tests;
