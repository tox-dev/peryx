use serde::{Deserialize, Serialize};

use peryx_storage::meta::{MetaError, MetaStore};

use crate::{ChangelogEntry, ChangelogPage, ChangelogPageError};

/// One recorded mutation in the [`MetaStore`] journal: the append-only changelog that makes peryx
/// an origin others can replicate from. `serial` orders entries; the rest names what changed.
///
/// The neutral serial counter lives in the store, so a `PyPI` publish builds this entry with a
/// placeholder `serial` and lets [`commit_driver_txn`] allocate the authoritative one - see
/// [`publish_file_if`](super::publish_file_if).
///
/// [`MetaStore`]: peryx_storage::meta::MetaStore
/// [`commit_driver_txn`]: peryx_storage::meta::MetaStore::commit_driver_txn
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// Read one journal snapshot and convert its records to Warehouse tuple values.
///
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

/// Read a bounded journal snapshot and decode its opaque values as `PyPI` entries.
///
/// Storage owns the serial, so this replaces the serialized placeholder with the record key.
///
/// # Errors
/// Returns a store error if the snapshot cannot be read or an entry cannot be decoded.
pub fn read_journal_entries(meta: &MetaStore, after: u64, limit: usize) -> Result<JournalSnapshot, MetaError> {
    let (current_serial, records) = meta.journal_page_after(after, limit)?;
    let entries = records
        .into_iter()
        .map(|record| {
            let mut entry = serde_json::from_slice::<JournalEntry>(&record.payload)?;
            entry.serial = record.serial;
            Ok(entry)
        })
        .collect::<Result<_, serde_json::Error>>()?;
    Ok(JournalSnapshot {
        current_serial,
        entries,
    })
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
