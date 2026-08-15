use axum::http::{Method, StatusCode};
use peryx_storage::meta::{AccountingClass, MetaStore, NewQuotaReservation, QuotaLimits};
use rstest::rstest;
use tempfile::TempDir;

use super::{app_with_journal, auth, oci_digest, send_body, writable_index};
use crate::name::Reference;
use crate::outbox::OciMutation;
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
            reservation: None,
            journal: true,
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
            reservation: None,
            journal: false,
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
fn test_trash_tag_records_a_trash_tag_operation() {
    let (_dir, meta) = store();
    store::record_manifest(&meta, "store", "app", "sha256:abc", &manifest()).unwrap();
    store::put_tag(&meta, "store", "app", "latest", "sha256:abc").unwrap();

    store::trash_tag(&meta, "store", "app", "latest", &info(), true).unwrap();

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

    store::trash_manifest(&meta, "store", "app", "sha256:abc", &info(), true).unwrap();

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
    store::trash_tag(&meta, "store", "app", "latest", &info(), false).unwrap();

    store::restore_tag(&meta, "store", "app", "latest", true).unwrap();

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
    store::trash_manifest(&meta, "store", "app", "sha256:abc", &info(), false).unwrap();

    store::restore_manifest(&meta, "store", "app", "sha256:abc", true).unwrap();

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

    let digest = store::trash_tag(&meta, "store", "app", "absent", &info(), true).unwrap();

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
        b"{}".to_vec(),
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
