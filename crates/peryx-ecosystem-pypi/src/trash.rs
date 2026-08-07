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
mod tests {
    use std::collections::BTreeMap;

    use peryx_storage::meta::MetaStore;

    use super::trash_records;
    use crate::store::PypiStore as _;
    use crate::upload::{TrashInfo, Uploaded};
    use crate::{CoreMetadata, File, Provenance, Yanked};

    fn store() -> (tempfile::TempDir, MetaStore) {
        let dir = tempfile::tempdir().unwrap();
        let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
        (dir, meta)
    }

    fn seed(meta: &MetaStore, index: &str, project: &str, filename: &str, trashed: Option<TrashInfo>) {
        let uploaded = Uploaded {
            version: "1.0".to_owned(),
            file: File {
                filename: filename.to_owned(),
                url: format!("https://files/{filename}"),
                hashes: BTreeMap::from([("sha256".to_owned(), "deadbeef".to_owned())]),
                requires_python: None,
                size: Some(1_024),
                upload_time: Some("2020-01-01T00:00:00Z".to_owned()),
                yanked: Yanked::No,
                core_metadata: CoreMetadata::Absent,
                dist_info_metadata: CoreMetadata::Absent,
                gpg_sig: None,
                provenance: Provenance::Absent,
            },
            trashed,
        };
        meta.put_upload(index, project, filename, &serde_json::to_vec(&uploaded).unwrap())
            .unwrap();
    }

    fn trash() -> TrashInfo {
        TrashInfo {
            deleted_at_unix: 100,
            actor: Some("alice".to_owned()),
            reason: Some("bad build".to_owned()),
        }
    }

    #[test]
    fn test_trash_records_returns_only_soft_deleted_files() {
        let (_dir, meta) = store();
        seed(&meta, "hosted", "flask", "flask-1.0.whl", Some(trash()));
        seed(&meta, "hosted", "flask", "flask-2.0.whl", None);

        let records = trash_records(&meta, "hosted").unwrap();

        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.ecosystem, crate::ECOSYSTEM);
        assert_eq!(record.repository, "hosted");
        assert_eq!(record.name, "flask");
        assert_eq!(record.reference.as_deref(), Some("flask-1.0.whl"));
        assert_eq!(record.digest.as_deref(), Some("sha256:deadbeef"));
        assert_eq!(record.reason.as_deref(), Some("bad build"));
        assert_eq!(record.actor.as_deref(), Some("alice"));
        assert_eq!(record.deleted_at_unix, 100);
        assert!(record.retained);
    }

    #[test]
    fn test_trash_records_scope_to_the_index() {
        let (_dir, meta) = store();
        seed(&meta, "hosted", "flask", "flask-1.0.whl", Some(trash()));
        seed(&meta, "other", "flask", "flask-1.0.whl", Some(trash()));

        assert_eq!(trash_records(&meta, "hosted").unwrap().len(), 1);
    }

    #[test]
    fn test_trash_records_rejects_a_corrupt_upload_record() {
        let (_dir, meta) = store();
        meta.put_upload("hosted", "flask", "flask-1.0.whl", b"not json")
            .unwrap();

        let error = trash_records(&meta, "hosted").unwrap_err();

        assert!(error.contains("corrupt upload record"), "{error}");
    }

    #[test]
    fn test_trash_records_skips_a_malformed_upload_key() {
        let (_dir, meta) = store();
        meta.put_driver_value("pypi\u{0}u\u{0}hosted/malformed", b"{}").unwrap();

        assert!(trash_records(&meta, "hosted").unwrap().is_empty());
    }
}
