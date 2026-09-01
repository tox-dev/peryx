use axum::http::{Method, StatusCode};
use peryx_ha::{ArtifactPlacement, ArtifactSource};
use peryx_storage::meta::{AccountingClass, MetaStore, NewQuotaReservation, QuotaLimits, QuotaReservationRecord};
use rstest::rstest;
use tempfile::TempDir;

use super::{app_with_journal, auth, oci_digest, send_body, send_with, writable_index};
use crate::name::Reference;
use crate::outbox::OciMutation;
use crate::registry::ServeError;
use crate::store::{self, Manifest};
use crate::{quota, store::TrashInfo};

const TOKEN: &str = "s3cret";
const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

fn store() -> (TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, meta)
}

fn manifest() -> Manifest {
    Manifest {
        media_type: MANIFEST_TYPE.to_owned(),
        bytes: b"{}".to_vec(),
    }
}

fn info() -> TrashInfo {
    TrashInfo {
        deleted_at_unix: 100,
        actor: Some("alice".to_owned()),
        reason: Some("bad build".to_owned()),
    }
}

fn only_op(meta: &MetaStore) -> OciMutation {
    let records = meta.journal_after(0, 100).unwrap();
    assert_eq!(records.len(), 1, "one mutation records one outbox entry");
    assert!(
        !records[0].mutations.is_empty(),
        "the changed rows commit in the same transaction as the entry"
    );
    serde_json::from_slice(&records[0].payload).unwrap()
}

#[rstest]
#[case::by_tag(Reference::Tag("latest".to_owned()), Some("latest".to_owned()))]
#[case::by_digest(Reference::Digest("sha256:abc".to_owned()), None)]
fn test_publish_manifest_records_the_reference_it_named(#[case] reference: Reference, #[case] tag: Option<String>) {
    let (_dir, meta) = store();

    quota::publish_manifest(
        &meta,
        quota::ManifestCommit {
            index: "store",
            repo: "app",
            canonical: "sha256:abc",
            manifest: &manifest(),
            reference: &reference,
            referrer: None,
            reservation: None,
            journal: true,
            webhook: None,
            operation: None,
        },
    )
    .unwrap();

    assert_eq!(
        only_op(&meta),
        OciMutation::PublishManifest {
            index: "store".to_owned(),
            repo: "app".to_owned(),
            digest: "sha256:abc".to_owned(),
            tag,
        }
    );
}

fn referrer() -> store::Referrer {
    store::Referrer {
        subject: "sha256:subject".to_owned(),
        descriptor: br#"{"digest":"sha256:abc"}"#.to_vec(),
    }
}

fn publish(
    meta: &MetaStore,
    referrer: Option<&store::Referrer>,
    reservation: Option<QuotaReservationRecord>,
) -> Result<bool, ServeError> {
    quota::publish_manifest(
        meta,
        quota::ManifestCommit {
            index: "store",
            repo: "app",
            canonical: "sha256:abc",
            manifest: &manifest(),
            reference: &Reference::Tag("latest".to_owned()),
            referrer,
            reservation,
            journal: true,
            webhook: None,
            operation: None,
        },
    )
    .map(|committed| committed.value)
}

/// Everything a reader resolves from a published manifest: the manifest itself, where its bytes live,
/// and what its subject lists. Publication either brings all three into existence or none of them.
fn resolved(meta: &MetaStore) -> (Option<Manifest>, Option<ArtifactPlacement>, Vec<Vec<u8>>) {
    (
        store::get_manifest(meta, "sha256:abc").unwrap(),
        meta.get_artifact_placement("sha256:abc").unwrap(),
        store::list_referrers(meta, "store", "app", "sha256:subject").unwrap(),
    )
}

fn reserve(meta: &MetaStore, created_at_unix: i64) -> QuotaReservationRecord {
    meta.reserve_quota(
        NewQuotaReservation {
            repository: "store",
            resource: Some("app"),
            group: Some("latest"),
            digest: "sha256:abc",
            bytes: 2,
            class: AccountingClass::Hosted,
            created_at_unix,
        },
        QuotaLimits::default(),
    )
    .unwrap()
}

#[rstest]
#[case::with_a_subject(Some(referrer()), vec![referrer().descriptor])]
#[case::without_a_subject(None, Vec::new())]
fn test_publication_commits_the_placement_and_any_referrer_row(
    #[case] row: Option<store::Referrer>,
    #[case] listed: Vec<Vec<u8>>,
) {
    let (_dir, meta) = store();

    publish(&meta, row.as_ref(), None).unwrap();

    assert_eq!(
        resolved(&meta),
        (
            Some(manifest()),
            Some(ArtifactPlacement::record(ArtifactSource::Hosted, true)),
            listed
        )
    );
}

#[test]
fn test_a_publication_that_cannot_settle_exposes_nothing_and_a_retry_repairs_it() {
    let (_dir, meta) = store();
    let released = reserve(&meta, 100);
    meta.release_quota_reservation(released.id).unwrap();

    let aborted = publish(&meta, Some(&referrer()), Some(released));

    assert!(aborted.is_err(), "an unavailable reservation aborts the publication");
    assert_eq!(
        resolved(&meta),
        (None, None, Vec::new()),
        "the manifest and its derived rows roll back together"
    );

    publish(&meta, Some(&referrer()), Some(reserve(&meta, 101))).unwrap();

    assert_eq!(
        only_op(&meta),
        OciMutation::PublishManifest {
            index: "store".to_owned(),
            repo: "app".to_owned(),
            digest: "sha256:abc".to_owned(),
            tag: Some("latest".to_owned()),
        }
    );
    assert_eq!(
        resolved(&meta),
        (
            Some(manifest()),
            Some(ArtifactPlacement::record(ArtifactSource::Hosted, true)),
            vec![referrer().descriptor]
        )
    );
}

#[test]
fn test_none_mode_publish_records_no_outbox_entry() {
    let (_dir, meta) = store();

    quota::publish_manifest(
        &meta,
        quota::ManifestCommit {
            index: "store",
            repo: "app",
            canonical: "sha256:abc",
            manifest: &manifest(),
            reference: &Reference::Digest("sha256:abc".to_owned()),
            referrer: None,
            reservation: None,
            journal: false,
            webhook: None,
            operation: None,
        },
    )
    .unwrap();

    assert_eq!(meta.current_serial().unwrap(), 0, "none mode records nothing");
    assert!(
        store::get_manifest(&meta, "sha256:abc").unwrap().is_some(),
        "the manifest still commits without an entry"
    );
}

#[test]
fn test_blob_membership_records_a_mount_operation() {
    let (_dir, meta) = store();

    quota::commit_blob_membership(&meta, "store", "app", "sha256:layer", None, None, true).unwrap();

    assert_eq!(
        only_op(&meta),
        OciMutation::MountBlob {
            index: "store".to_owned(),
            repo: "app".to_owned(),
            digest: "sha256:layer".to_owned(),
        }
    );
    assert!(store::blob_is_member(&meta, "store", "app", "sha256:layer").unwrap());
}

#[test]
fn test_blob_deletion_records_an_unmount_operation() {
    let (_dir, meta) = store();
    quota::commit_blob_membership(&meta, "store", "app", "sha256:layer", None, None, false).unwrap();

    assert!(quota::release_blob_membership(&meta, "store", "app", "sha256:layer", None, true).unwrap());

    assert_eq!(
        only_op(&meta),
        OciMutation::UnmountBlob {
            index: "store".to_owned(),
            repo: "app".to_owned(),
            digest: "sha256:layer".to_owned(),
        }
    );
    assert!(!store::blob_is_member(&meta, "store", "app", "sha256:layer").unwrap());
}

#[test]
fn test_deleting_an_absent_blob_membership_records_no_outbox_entry() {
    let (_dir, meta) = store();

    let removed = quota::release_blob_membership(&meta, "store", "app", "sha256:layer", None, true).unwrap();

    assert!(!removed, "no membership was present to remove");
    assert_eq!(meta.current_serial().unwrap(), 0, "a replayed removal records nothing");
}

#[test]
fn test_none_mode_blob_deletion_records_no_outbox_entry() {
    let (_dir, meta) = store();
    quota::commit_blob_membership(&meta, "store", "app", "sha256:layer", None, None, false).unwrap();

    assert!(quota::release_blob_membership(&meta, "store", "app", "sha256:layer", None, false).unwrap());

    assert_eq!(meta.current_serial().unwrap(), 0, "none mode records nothing");
    assert!(!store::blob_is_member(&meta, "store", "app", "sha256:layer").unwrap());
}

#[test]
fn test_trash_tag_records_a_trash_tag_operation() {
    let (_dir, meta) = store();
    store::record_manifest(&meta, "store", "app", "sha256:abc", &manifest()).unwrap();
    store::put_tag(&meta, "store", "app", "latest", "sha256:abc").unwrap();

    store::trash_tag(&meta, "store", "app", "latest", &info(), true, |_| None).unwrap();

    assert_eq!(
        only_op(&meta),
        OciMutation::TrashTag {
            index: "store".to_owned(),
            repo: "app".to_owned(),
            tag: "latest".to_owned(),
            digest: "sha256:abc".to_owned(),
        }
    );
}

#[test]
fn test_trash_manifest_records_the_captured_tags() {
    let (_dir, meta) = store();
    store::record_manifest(&meta, "store", "app", "sha256:abc", &manifest()).unwrap();
    store::put_tag(&meta, "store", "app", "1.0", "sha256:abc").unwrap();
    store::put_tag(&meta, "store", "app", "latest", "sha256:abc").unwrap();

    store::trash_manifest(&meta, "store", "app", "sha256:abc", &info(), true, None).unwrap();

    assert_eq!(
        only_op(&meta),
        OciMutation::TrashManifest {
            index: "store".to_owned(),
            repo: "app".to_owned(),
            digest: "sha256:abc".to_owned(),
            tags: vec!["1.0".to_owned(), "latest".to_owned()],
        }
    );
}

#[test]
fn test_restore_tag_records_a_restore_operation() {
    let (_dir, meta) = store();
    store::record_manifest(&meta, "store", "app", "sha256:abc", &manifest()).unwrap();
    store::put_tag(&meta, "store", "app", "latest", "sha256:abc").unwrap();
    store::trash_tag(&meta, "store", "app", "latest", &info(), false, |_| None).unwrap();

    store::restore_tag(&meta, "store", "app", "latest", true, |_| None).unwrap();

    assert_eq!(
        only_op(&meta),
        OciMutation::RestoreTag {
            index: "store".to_owned(),
            repo: "app".to_owned(),
            tag: "latest".to_owned(),
            digest: "sha256:abc".to_owned(),
        }
    );
}

#[test]
fn test_restore_manifest_records_the_relit_tags() {
    let (_dir, meta) = store();
    store::record_manifest(&meta, "store", "app", "sha256:abc", &manifest()).unwrap();
    store::put_tag(&meta, "store", "app", "1.0", "sha256:abc").unwrap();
    store::put_tag(&meta, "store", "app", "latest", "sha256:abc").unwrap();
    store::trash_manifest(&meta, "store", "app", "sha256:abc", &info(), false, None).unwrap();

    store::restore_manifest(&meta, "store", "app", "sha256:abc", true, None).unwrap();

    assert_eq!(
        only_op(&meta),
        OciMutation::RestoreManifest {
            index: "store".to_owned(),
            repo: "app".to_owned(),
            digest: "sha256:abc".to_owned(),
            tags: vec!["1.0".to_owned(), "latest".to_owned()],
        }
    );
}

#[test]
fn test_no_op_deletion_records_no_outbox_entry() {
    let (_dir, meta) = store();

    let digest = store::trash_tag(&meta, "store", "app", "absent", &info(), true, |_| None).unwrap();

    assert!(digest.is_none(), "no tag was present to trash");
    assert_eq!(meta.current_serial().unwrap(), 0, "a no-op records nothing");
}

#[test]
fn test_aborted_transaction_leaves_no_row_or_outbox_entry() {
    let (_dir, meta) = store();
    let reservation = meta
        .reserve_quota(
            NewQuotaReservation {
                repository: "store",
                resource: Some("app"),
                group: None,
                digest: "sha256:layer",
                bytes: 5,
                class: AccountingClass::Hosted,
                created_at_unix: 100,
            },
            QuotaLimits::default(),
        )
        .unwrap();
    meta.release_quota_reservation(reservation.id).unwrap();

    let outcome = quota::commit_blob_membership(&meta, "store", "app", "sha256:layer", Some(reservation), None, true);

    assert!(outcome.is_err(), "an unavailable reservation aborts the commit");
    assert!(
        !store::blob_is_member(&meta, "store", "app", "sha256:layer").unwrap(),
        "the membership row rolls back with the transaction"
    );
    assert_eq!(meta.current_serial().unwrap(), 0, "no outbox entry survives the abort");
}

#[test]
fn test_journal_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    {
        let meta = MetaStore::open(&path).unwrap();
        quota::commit_blob_membership(&meta, "store", "app", "sha256:layer", None, None, true).unwrap();
    }

    let meta = MetaStore::open(&path).unwrap();

    assert_eq!(
        only_op(&meta),
        OciMutation::MountBlob {
            index: "store".to_owned(),
            repo: "app".to_owned(),
            digest: "sha256:layer".to_owned(),
        }
    );
}

#[tokio::test]
async fn test_hosted_push_journals_the_blob_and_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = app_with_journal(&dir, vec![writable_index("store", "store", true, TOKEN)], true);
    let blob = b"a-real-layer-of-bytes";
    let digest = oci_digest(blob);

    let (status, _, _) = send_body(
        &app,
        Method::POST,
        &format!("/v2/store/app/blobs/uploads/?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        blob.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _, _) = send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/1.0",
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        format!(
            r#"{{"schemaVersion":2,"mediaType":"{MANIFEST_TYPE}","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{digest}","size":{}}},"layers":[]}}"#,
            blob.len(),
        )
        .into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let ops: Vec<OciMutation> = state
        .serving
        .meta
        .journal_after(0, 100)
        .unwrap()
        .iter()
        .map(|record| serde_json::from_slice(&record.payload).unwrap())
        .collect();
    assert!(
        ops.iter()
            .any(|op| matches!(op, OciMutation::MountBlob { digest, .. } if digest == &oci_digest(blob))),
        "the pushed blob records a mount: {ops:?}"
    );
    assert!(
        ops.iter()
            .any(|op| matches!(op, OciMutation::PublishManifest { tag: Some(tag), .. } if tag == "1.0")),
        "the pushed manifest records a publish: {ops:?}"
    );
}

#[tokio::test]
async fn test_hosted_blob_delete_journals_the_unmount() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = app_with_journal(&dir, vec![writable_index("store", "store", true, TOKEN)], true);
    let blob = b"a-layer-that-gets-deleted";
    let digest = oci_digest(blob);
    let (status, _, _) = send_body(
        &app,
        Method::POST,
        &format!("/v2/store/app/blobs/uploads/?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        blob.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let pushed = state.serving.meta.current_serial().unwrap();

    let (status, _, _) = send_with(
        &app,
        Method::DELETE,
        &format!("/v2/store/app/blobs/{digest}"),
        &[("authorization", &auth(TOKEN))],
    )
    .await;

    assert_eq!(status, StatusCode::ACCEPTED, "a standalone delete keeps its 202");
    let ops: Vec<OciMutation> = state
        .serving
        .meta
        .journal_after(pushed, 100)
        .unwrap()
        .iter()
        .map(|record| serde_json::from_slice(&record.payload).unwrap())
        .collect();
    assert_eq!(
        ops,
        vec![OciMutation::UnmountBlob {
            index: "store".to_owned(),
            repo: "app".to_owned(),
            digest,
        }]
    );
}
