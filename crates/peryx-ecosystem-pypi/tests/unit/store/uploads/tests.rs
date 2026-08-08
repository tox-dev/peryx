use std::collections::BTreeMap;

use peryx_storage::meta::{AccountingClass, NewQuotaReservation};

use super::{
    Guard, MetaError, MetaStore, MetadataSibling, PromotedRelease, ProvenanceSibling, PublishError, PublishedFile,
    UploadMutation, map_publish_error, override_key, upload_key,
};
use crate::store::{PypiStore as _, read_journal_entries};
use crate::upload::UploadStoreError;

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, meta)
}

fn published() -> PublishedFile<'static> {
    PublishedFile {
        index: "hosted",
        normalized: "flask",
        display: "Flask",
        filename: "flask-1.0.whl",
        artifact_sha256: "artifact-sha",
        artifact_size: 8,
        record: b"record",
        version: "1.0",
        submitted_at_unix: 123,
        metadata: Some(MetadataSibling {
            url: "uploaded",
            metadata_sha256: "metadata-sha",
            size: 8,
            source: "hosted",
        }),
        provenance: None,
        quota: None,
    }
}

#[test]
fn test_publish_file_if_commit_writes_record_sibling_project_and_serial() {
    let (_dir, meta) = store();

    let wrote = meta
        .publish_file_if(&published(), |existing| {
            assert!(existing.is_none(), "a first publish sees no prior record");
            Ok::<_, MetaError>(Guard::Commit)
        })
        .unwrap();

    assert!(wrote);
    assert_eq!(
        meta.get_driver_value(&upload_key("hosted", "flask", "flask-1.0.whl"))
            .unwrap()
            .as_deref(),
        Some(b"record".as_slice())
    );
    assert!(
        meta.get_metadata("artifact-sha").unwrap().is_some(),
        "the sibling row is written"
    );
    assert_eq!(meta.get_project("hosted", "flask").unwrap().as_deref(), Some("Flask"));
    let journal = read_journal_entries(&meta, 0, 1).unwrap().entries.pop().unwrap();
    assert_eq!(journal.action, "add-file");
    assert_eq!(journal.version.as_deref(), Some("1.0"));
    assert_eq!(journal.filename.as_deref(), Some("flask-1.0.whl"));
    assert_eq!(journal.submitted_at_unix, 123);
}

#[test]
fn test_publish_file_if_commits_quota_with_a_new_record() {
    let (_dir, meta) = store();
    let reservation = reservation(&meta);

    let wrote = meta
        .publish_file_if(
            &PublishedFile {
                quota: Some(&reservation),
                ..published()
            },
            |_existing| Ok::<_, MetaError>(Guard::Commit),
        )
        .unwrap();

    assert!(wrote);
    assert_eq!(
        meta.quota_project_usage("hosted", "flask")
            .unwrap()
            .file_bytes
            .committed,
        8
    );
}

#[test]
fn test_publish_file_if_releases_quota_for_a_duplicate() {
    let (_dir, meta) = store();
    meta.publish_file_if(&published(), |_existing| Ok::<_, MetaError>(Guard::Commit))
        .unwrap();
    let reservation = reservation(&meta);

    let wrote = meta
        .publish_file_if(
            &PublishedFile {
                quota: Some(&reservation),
                ..published()
            },
            |_existing| Ok::<_, MetaError>(Guard::Skip),
        )
        .unwrap();

    assert!(!wrote);
    assert_eq!(meta.quota_reservation(reservation.id).unwrap(), None);
    assert_eq!(
        meta.quota_project_usage("hosted", "flask").unwrap().file_bytes,
        peryx_storage::meta::QuotaValue::default()
    );
}

#[test]
fn test_publish_file_if_leaves_quota_pending_after_a_guard_error() {
    let (_dir, meta) = store();
    let reservation = reservation(&meta);

    let result = meta.publish_file_if(
        &PublishedFile {
            quota: Some(&reservation),
            ..published()
        },
        |_existing| Err::<Guard, _>(MetaError::DriverPrecondition("conflict".to_owned())),
    );

    assert!(matches!(result, Err(MetaError::DriverPrecondition(reason)) if reason == "conflict"));
    assert_eq!(
        meta.quota_project_usage("hosted", "flask").unwrap().file_bytes.reserved,
        8
    );
}

#[test]
fn test_publish_file_if_rejects_a_used_quota_reservation() {
    let (_dir, meta) = store();
    let reservation = reservation(&meta);
    meta.commit_quota_reservation(reservation.id).unwrap();

    let result = meta.publish_file_if(
        &PublishedFile {
            quota: Some(&reservation),
            ..published()
        },
        |_existing| Ok::<_, MetaError>(Guard::Commit),
    );

    assert!(matches!(result, Err(MetaError::DriverPrecondition(reason)) if reason.contains("already committed")));
    assert!(
        meta.get_driver_value(&upload_key("hosted", "flask", "flask-1.0.whl"))
            .unwrap()
            .is_none()
    );
}

#[test]
fn test_publish_file_if_preserves_quota_store_errors() {
    let error =
        map_publish_error::<UploadStoreError>(PublishError::from(MetaError::DriverPrecondition("store".to_owned())));

    assert!(matches!(error, UploadStoreError::Meta(MetaError::DriverPrecondition(reason)) if reason == "store"));
}

#[test]
fn test_publish_file_if_preserves_driver_store_errors() {
    let error = map_publish_error::<MetaError>(PublishError::from(MetaError::DriverPrecondition("store".to_owned())));

    assert!(matches!(error, MetaError::DriverPrecondition(reason) if reason == "store"));
}

fn reservation(meta: &MetaStore) -> peryx_storage::meta::QuotaReservationRecord {
    meta.reserve_project_quota(
        NewQuotaReservation {
            repository: "hosted",
            project: Some("flask"),
            version: Some("1.0"),
            digest: "artifact-sha",
            bytes: 8,
            class: AccountingClass::Hosted,
            created_at_unix: 123,
        },
        8,
        false,
    )
    .unwrap()
}

#[test]
fn test_publish_file_if_commit_without_a_metadata_sibling_writes_no_sibling() {
    let (_dir, meta) = store();

    let wrote = meta
        .publish_file_if(
            &PublishedFile {
                metadata: None,
                ..published()
            },
            |_existing| Ok::<_, MetaError>(Guard::Commit),
        )
        .unwrap();

    assert!(wrote);
    assert!(
        meta.get_metadata("artifact-sha").unwrap().is_none(),
        "a file without metadata records no sibling row"
    );
    assert_eq!(
        meta.get_driver_value(&upload_key("hosted", "flask", "flask-1.0.whl"))
            .unwrap()
            .as_deref(),
        Some(b"record".as_slice())
    );
}

#[test]
fn test_publish_file_if_records_artifact_and_metadata_blobs() {
    let (_dir, meta) = store();

    meta.publish_file_if(&published(), |_existing| Ok::<_, MetaError>(Guard::Commit))
        .unwrap();

    assert_eq!(
        meta.journal_after(0, 1).unwrap()[0].blobs,
        vec![
            peryx_storage::meta::DriverBlobReference {
                sha256: "artifact-sha".to_owned(),
                size: 8,
            },
            peryx_storage::meta::DriverBlobReference {
                sha256: "metadata-sha".to_owned(),
                size: 8,
            },
        ]
    );
}

#[test]
fn test_publish_file_if_writes_the_provenance_row_and_references_its_blob() {
    let (_dir, meta) = store();

    meta.publish_file_if(
        &PublishedFile {
            provenance: Some(ProvenanceSibling {
                provenance_sha256: "provenance-sha",
                size: 16,
            }),
            ..published()
        },
        |_existing| Ok::<_, MetaError>(Guard::Commit),
    )
    .unwrap();

    assert_eq!(
        meta.get_provenance("artifact-sha").unwrap(),
        Some(("provenance-sha".to_owned(), 16))
    );
    assert!(
        meta.journal_after(0, 1).unwrap()[0]
            .blobs
            .contains(&peryx_storage::meta::DriverBlobReference {
                sha256: "provenance-sha".to_owned(),
                size: 16,
            }),
        "the provenance blob is recorded so a purge keeps it"
    );
}

#[test]
fn test_publish_file_if_without_provenance_writes_no_provenance_row() {
    let (_dir, meta) = store();

    meta.publish_file_if(&published(), |_existing| Ok::<_, MetaError>(Guard::Commit))
        .unwrap();

    assert!(meta.get_provenance("artifact-sha").unwrap().is_none());
}

#[test]
fn test_publish_file_if_skip_leaves_the_store_unchanged() {
    let (_dir, meta) = store();

    let wrote = meta
        .publish_file_if(&published(), |_existing| Ok::<_, MetaError>(Guard::Skip))
        .unwrap();

    assert!(!wrote);
    assert!(
        meta.get_driver_value(&upload_key("hosted", "flask", "flask-1.0.whl"))
            .unwrap()
            .is_none()
    );
    assert_eq!(meta.current_serial().unwrap(), 0, "a skipped publish records no serial");
}

#[test]
fn test_publish_file_if_propagates_a_guard_rejection_without_writing() {
    let (_dir, meta) = store();

    let result = meta.publish_file_if(&published(), |_existing| {
        Err::<Guard, _>(MetaError::from(
            serde_json::from_str::<serde_json::Value>("{").unwrap_err(),
        ))
    });

    assert!(result.is_err());
    assert!(
        meta.get_driver_value(&upload_key("hosted", "flask", "flask-1.0.whl"))
            .unwrap()
            .is_none()
    );
}

#[test]
fn test_promote_files_checked_writes_the_release_project_and_journal() {
    let (_dir, meta) = store();
    let records = vec![(
        "flask-1.0.whl".to_owned(),
        "artifact-sha".to_owned(),
        br#"{"version":"1.0"}"#.to_vec(),
    )];
    let blob_sizes = BTreeMap::from([("artifact-sha".to_owned(), 8)]);

    let written = meta
        .promote_files_checked(
            &PromotedRelease {
                index: "hosted",
                normalized: "flask",
                display: "Flask",
                records: &records,
                blob_sizes: &blob_sizes,
                submitted_at_unix: 123,
            },
            |filename, digest, existing| {
                assert_eq!((filename, digest, existing), ("flask-1.0.whl", "artifact-sha", None));
                Ok::<_, MetaError>(Guard::Commit)
            },
        )
        .unwrap();

    assert_eq!(written, 1);
    assert_eq!(
        meta.get_driver_value(&upload_key("hosted", "flask", "flask-1.0.whl"))
            .unwrap()
            .as_deref(),
        Some(br#"{"version":"1.0"}"#.as_slice())
    );
    assert_eq!(meta.get_project("hosted", "flask").unwrap().as_deref(), Some("Flask"));
    let batch = meta.journal_after(0, 1).unwrap().pop().unwrap();
    assert_eq!(
        batch.blobs,
        vec![peryx_storage::meta::DriverBlobReference {
            sha256: "artifact-sha".to_owned(),
            size: 8,
        }]
    );
    let journal = read_journal_entries(&meta, 0, 1).unwrap().entries.pop().unwrap();
    assert_eq!(journal.action, "add-file");
    assert_eq!(journal.version.as_deref(), Some("1.0"));
    assert_eq!(journal.filename.as_deref(), Some("flask-1.0.whl"));
    assert_eq!(journal.submitted_at_unix, 123);
}

#[test]
fn test_scan_upload_records_visits_each_row() {
    let (_dir, meta) = store();
    meta.put_upload("hosted", "flask", "flask-1.0.whl", b"upload").unwrap();
    let mut seen = Vec::new();
    meta.scan_upload_records(|key, value| {
        seen.push((key.to_owned(), value.to_vec()));
        Ok::<(), std::io::Error>(())
    })
    .unwrap();
    assert_eq!(
        seen,
        vec![("hosted/flask/flask-1.0.whl".to_owned(), b"upload".to_vec())]
    );
}

#[test]
fn test_scan_override_records_visits_valid_and_skips_non_utf8() {
    let (_dir, meta) = store();
    meta.put_override("hosted", "flask", "flask-1.0.whl", "hidden", 123)
        .unwrap();
    meta.put_driver_value(&override_key("hosted", "flask", "bad.whl"), &[0xff, 0xfe])
        .unwrap();
    let mut seen = Vec::new();
    meta.scan_override_records(|key, value| {
        seen.push((key.to_owned(), value.to_owned()));
        Ok::<(), std::io::Error>(())
    })
    .unwrap();
    assert_eq!(
        seen,
        vec![("hosted/flask/flask-1.0.whl".to_owned(), "hidden".to_owned())]
    );
}

#[test]
fn test_mutate_uploads_journals_the_action_for_each_rewritten_record() {
    let (_dir, meta) = store();
    meta.put_upload("hosted", "flask", "flask-1.0.whl", b"a").unwrap();
    meta.put_upload("hosted", "flask", "flask-2.0.whl", b"b").unwrap();

    let changed = meta
        .mutate_uploads("hosted", "flask", "yank", 123, |_filename, _record| {
            Ok::<_, MetaError>(UploadMutation::Replace(b"yanked".to_vec()))
        })
        .unwrap();

    assert_eq!(changed, 2);
    assert_eq!(
        meta.current_serial().unwrap(),
        2,
        "each rewritten record allocates its own serial"
    );
    assert_eq!(
        read_journal_entries(&meta, 0, 2)
            .unwrap()
            .entries
            .into_iter()
            .map(|entry| (entry.action, entry.version, entry.filename))
            .collect::<Vec<_>>(),
        [
            (
                "yank".to_owned(),
                Some("1.0".to_owned()),
                Some("flask-1.0.whl".to_owned()),
            ),
            (
                "yank".to_owned(),
                Some("2.0".to_owned()),
                Some("flask-2.0.whl".to_owned()),
            ),
        ]
    );
}

#[test]
fn test_mutate_uploads_journals_only_the_removed_record_and_keeps_the_rest() {
    let (_dir, meta) = store();
    meta.put_upload("hosted", "flask", "flask-1.0.whl", b"a").unwrap();
    meta.put_upload("hosted", "flask", "flask-2.0.whl", b"b").unwrap();

    let changed = meta
        .mutate_uploads("hosted", "flask", "delete-file", 123, |filename, _record| {
            Ok::<_, MetaError>(if filename == "flask-1.0.whl" {
                UploadMutation::Delete
            } else {
                UploadMutation::Keep
            })
        })
        .unwrap();

    assert_eq!(changed, 1);
    assert!(
        meta.get_driver_value(&upload_key("hosted", "flask", "flask-1.0.whl"))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        meta.current_serial().unwrap(),
        1,
        "only the removed record is journaled"
    );
    assert_eq!(
        read_journal_entries(&meta, 0, 1).unwrap().entries[0].version.as_deref(),
        Some("1.0")
    );
}

#[test]
fn test_mutate_uploads_that_keeps_every_record_journals_nothing() {
    let (_dir, meta) = store();
    meta.put_upload("hosted", "flask", "flask-1.0.whl", b"a").unwrap();

    let changed = meta
        .mutate_uploads("hosted", "flask", "yank", 123, |_filename, _record| {
            Ok::<_, MetaError>(UploadMutation::Keep)
        })
        .unwrap();

    assert_eq!(changed, 0);
    assert_eq!(meta.current_serial().unwrap(), 0, "an all-keep batch records no serial");
}

#[test]
fn test_delete_upload_removes_the_record_and_journals_delete_file() {
    let (_dir, meta) = store();
    meta.put_upload("hosted", "flask", "flask-1.0.whl", b"record").unwrap();

    let existed = meta.delete_upload("hosted", "flask", "flask-1.0.whl", 123).unwrap();

    assert!(existed);
    assert!(
        meta.get_driver_value(&upload_key("hosted", "flask", "flask-1.0.whl"))
            .unwrap()
            .is_none()
    );
    assert_eq!(meta.current_serial().unwrap(), 1, "the deletion is journaled");
    assert_eq!(
        read_journal_entries(&meta, 0, 1).unwrap().entries[0].version.as_deref(),
        Some("1.0")
    );
}

#[test]
fn test_delete_upload_of_a_missing_record_journals_nothing() {
    let (_dir, meta) = store();

    let existed = meta.delete_upload("hosted", "flask", "flask-1.0.whl", 123).unwrap();

    assert!(!existed);
    assert_eq!(meta.current_serial().unwrap(), 0, "a no-op delete records no serial");
}

#[test]
fn test_put_override_hidden_journals_hide() {
    let (_dir, meta) = store();

    meta.put_override("hosted", "flask", "flask-1.0.whl", "hidden", 123)
        .unwrap();

    assert_eq!(
        meta.get_driver_value(&override_key("hosted", "flask", "flask-1.0.whl"))
            .unwrap()
            .as_deref(),
        Some(b"hidden".as_slice())
    );
    assert_eq!(meta.current_serial().unwrap(), 1, "the override is journaled");
    assert_eq!(
        read_journal_entries(&meta, 0, 1).unwrap().entries[0].version.as_deref(),
        Some("1.0")
    );
}

#[test]
fn test_put_override_yanked_journals_yank() {
    let (_dir, meta) = store();

    meta.put_override("hosted", "flask", "flask-1.0.whl", "yanked", 123)
        .unwrap();

    assert_eq!(
        meta.get_driver_value(&override_key("hosted", "flask", "flask-1.0.whl"))
            .unwrap()
            .as_deref(),
        Some(b"yanked".as_slice())
    );
    assert_eq!(meta.current_serial().unwrap(), 1, "the override is journaled");
}

#[test]
fn test_put_override_that_repeats_the_current_value_journals_nothing() {
    let (_dir, meta) = store();
    meta.put_override("hosted", "flask", "flask-1.0.whl", "yanked", 123)
        .unwrap();

    meta.put_override("hosted", "flask", "flask-1.0.whl", "yanked", 456)
        .unwrap();

    assert_eq!(
        meta.current_serial().unwrap(),
        1,
        "re-recording an identical override allocates no second serial"
    );
    assert_eq!(
        read_journal_entries(&meta, 0, 1).unwrap().entries[0].submitted_at_unix,
        123
    );
}

#[test]
fn test_delete_override_of_a_hidden_file_journals_restore() {
    let (_dir, meta) = store();
    meta.put_driver_value(&override_key("hosted", "flask", "flask-1.0.whl"), b"hidden")
        .unwrap();

    let existed = meta.delete_override("hosted", "flask", "flask-1.0.whl", 123).unwrap();

    assert!(existed);
    assert!(
        meta.get_driver_value(&override_key("hosted", "flask", "flask-1.0.whl"))
            .unwrap()
            .is_none()
    );
    assert_eq!(meta.current_serial().unwrap(), 1, "the restore is journaled");
    assert_eq!(
        read_journal_entries(&meta, 0, 1).unwrap().entries[0].version.as_deref(),
        Some("1.0")
    );
}

#[test]
fn test_delete_override_of_a_yanked_file_journals_unyank() {
    let (_dir, meta) = store();
    meta.put_driver_value(&override_key("hosted", "flask", "flask-1.0.whl"), b"yanked")
        .unwrap();

    let existed = meta.delete_override("hosted", "flask", "flask-1.0.whl", 123).unwrap();

    assert!(existed);
    assert!(
        meta.get_driver_value(&override_key("hosted", "flask", "flask-1.0.whl"))
            .unwrap()
            .is_none()
    );
    assert_eq!(meta.current_serial().unwrap(), 1, "the un-yank is journaled");
}

#[test]
fn test_delete_override_of_a_missing_file_journals_nothing() {
    let (_dir, meta) = store();

    let existed = meta.delete_override("hosted", "flask", "flask-1.0.whl", 123).unwrap();

    assert!(!existed);
    assert_eq!(meta.current_serial().unwrap(), 0, "a no-op reversal records no serial");
}
