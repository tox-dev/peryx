//! The `PyPI` source of soft-deleted records for the neutral trash-inspection view.
//!
//! A soft-deleted file keeps its upload record and blob until a purge, marked with
//! [`TrashInfo`](crate::upload::TrashInfo); this reads those markers from one index's indexed upload
//! rows and shapes them into neutral [`TrashRecord`]s. The scan touches only upload metadata, never a
//! blob or a policy run, so it stays bounded to the index it inspects.

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
                repository: index.to_owned(),
                name: project.to_owned(),
                reference: Some(uploaded.file.filename),
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
