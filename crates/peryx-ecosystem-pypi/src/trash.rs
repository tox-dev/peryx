//! Maps soft-deleted `PyPI` uploads into neutral trash records.

use peryx_core::TrashRecord;
use peryx_storage::meta::MetaStore;

use crate::error_message;
use crate::store::scan_upload_policy_snapshot;
use crate::upload::Uploaded;

/// Every soft-deleted file on `index`, newest state included, as neutral trash records.
///
/// A trashed record stays restorable while its blob is retained, and a purge removes the record and
/// its blob together, so a record the scan can see is retained until its recovery window closes.
///
/// # Errors
/// Returns a message when the store cannot be read or an upload record does not decode.
pub fn trash_records(meta: &MetaStore, index: &str) -> Result<Vec<TrashRecord>, String> {
    let mut records = Vec::new();
    scan_upload_policy_snapshot(meta, index, |key, bytes| {
        let Some((project, _filename)) = key.split_once('/') else {
            return Ok(());
        };
        let uploaded: Uploaded =
            serde_json::from_slice(bytes).map_err(|err| format!("corrupt upload record {key}: {err}"))?;
        if let Some(trash) = uploaded.trashed {
            records.push(TrashRecord {
                ecosystem: crate::ECOSYSTEM,
                repository: index.into(),
                resource: project.into(),
                artifact: Some(uploaded.file.filename.into()),
                digest: uploaded.file.hashes.get("sha256").map(|hex| format!("sha256:{hex}")),
                reason: trash.reason,
                actor: trash.actor,
                deleted_at_unix: trash.deleted_at_unix,
                retained: true,
            });
        }
        Ok::<(), String>(())
    })
    .map_err(error_message)?;
    Ok(records)
}

#[cfg(test)]
#[path = "../tests/unit/trash/tests.rs"]
mod tests;
