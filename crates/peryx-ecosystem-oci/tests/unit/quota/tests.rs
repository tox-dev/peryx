use peryx_storage::meta::{MetaStore, QuotaLimit, QuotaLimits, QuotaReservationState, QuotaUsage};

use super::{
    ManifestCommit, ReserveOutcome, commit_blob_membership, finalize, publish_manifest, quota_reservation,
    release_blob_membership, reserve,
};
use crate::name::Reference;
use crate::registry::ServeError;
use crate::store::{MAX_MEDIA_TYPE_BYTES, Manifest};
use crate::upload_session::UploadStore as _;

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, meta)
}

#[test]
fn test_reserve_admits_within_the_limit() {
    let (_dir, meta) = store();
    let request = quota_reservation("store", "app", None, "sha256:a", 4, 1);
    let limits = QuotaLimits {
        max_accounted_bytes: Some(8),
        ..QuotaLimits::default()
    };
    assert!(matches!(
        reserve(&meta, request, limits).unwrap(),
        ReserveOutcome::Admitted(_)
    ));
}

#[test]
fn test_reserve_rejects_over_the_limit_in_enforce_mode() {
    let (_dir, meta) = store();
    let request = quota_reservation("store", "app", None, "sha256:a", 9, 1);
    let limits = QuotaLimits {
        max_accounted_bytes: Some(8),
        ..QuotaLimits::default()
    };
    assert!(matches!(
        reserve(&meta, request, limits).unwrap(),
        ReserveOutcome::Rejected(violations) if violations == vec![QuotaLimit::AccountedBytes]
    ));
}

#[test]
fn test_reserve_maps_a_validation_fault_to_a_serve_error() {
    let (_dir, meta) = store();
    // Identity faults must not be reported as quota decisions.
    let long = "r".repeat(600);
    let request = quota_reservation(&long, "app", None, "sha256:a", 1, 1);
    assert!(reserve(&meta, request, QuotaLimits::default()).is_err());
}

#[test]
fn test_finalize_releases_the_reservation_after_a_driver_failure() {
    let (_dir, meta) = store();
    let request = quota_reservation("store", "app", None, "sha256:a", 4, 1);
    let record = meta.reserve_quota(request, QuotaLimits::default()).unwrap();
    let id = record.id;

    let result = finalize(
        &meta,
        Some(record),
        None,
        |()| true,
        |_txn| Err::<((), Vec<Vec<u8>>), _>(ServeError::Transport("driver write failed".to_owned())),
    );

    assert!(matches!(result, Err(ServeError::Transport(message)) if message == "driver write failed"));
    let usage = meta.quota_usage("store").unwrap();
    assert_eq!(
        (
            usage.accounted_bytes.committed,
            usage.accounted_bytes.reserved,
            usage.resources.committed,
            usage.resources.reserved,
            meta.quota_reservation(id).unwrap(),
        ),
        (0, 0, 0, 0, None)
    );
}

#[test]
fn test_blob_commits_one_of_two_prechecked_reservations() {
    let (_dir, meta) = store();
    let first = meta
        .reserve_quota(
            quota_reservation("store", "app", None, "sha256:a", 4, 1),
            QuotaLimits::default(),
        )
        .unwrap();
    let second = meta
        .reserve_quota(
            quota_reservation("store", "app", None, "sha256:a", 4, 2),
            QuotaLimits::default(),
        )
        .unwrap();
    meta.begin_upload("loser", "store", "app", 2).unwrap();

    commit_blob_membership(&meta, "store", "app", "sha256:a", Some(first), None, false).unwrap();
    commit_blob_membership(
        &meta,
        "store",
        "app",
        "sha256:a",
        Some(second.clone()),
        Some("loser"),
        false,
    )
    .unwrap();

    assert_eq!(meta.quota_reservation(second.id).unwrap(), None);
    assert_eq!(meta.upload_record("loser").unwrap(), None);
    assert_eq!(meta.quota_usage("store").unwrap().accounted_bytes.committed, 4);
    assert!(release_blob_membership(&meta, "store", "app", "sha256:a", None, false).unwrap());
    assert_eq!(meta.quota_usage("store").unwrap(), QuotaUsage::default());
}

#[test]
fn test_manifest_commits_one_of_two_prechecked_reservations() {
    let (_dir, meta) = store();
    let first = meta
        .reserve_quota(
            quota_reservation("store", "app", Some("stable"), "sha256:a", 4, 1),
            QuotaLimits::default(),
        )
        .unwrap();
    let second = meta
        .reserve_quota(
            quota_reservation("store", "app", Some("stable"), "sha256:a", 4, 2),
            QuotaLimits::default(),
        )
        .unwrap();
    let manifest = Manifest {
        media_type: "application/vnd.oci.image.manifest.v1+json".to_owned(),
        bytes: b"{}".to_vec(),
    };
    let reference = Reference::Tag("stable".to_owned());

    for reservation in [first, second.clone()] {
        publish_manifest(
            &meta,
            ManifestCommit {
                index: "store",
                repo: "app",
                canonical: "sha256:a",
                manifest: &manifest,
                reference: &reference,
                reservation: Some(reservation),
                journal: false,
                webhook: None,
            },
        )
        .unwrap();
    }

    assert_eq!(meta.quota_reservation(second.id).unwrap(), None);
    assert_eq!(
        (
            meta.quota_usage("store").unwrap().accounted_bytes.committed,
            meta.quota_resource_usage("store", "app").unwrap().groups.committed,
        ),
        (4, 1)
    );
}

#[test]
fn test_manifest_media_type_overflow_does_not_publish_the_manifest() {
    let (_dir, meta) = store();
    let result = publish_manifest(
        &meta,
        ManifestCommit {
            index: "store",
            repo: "app",
            canonical: "sha256:overflow",
            manifest: &Manifest {
                media_type: "a".repeat(MAX_MEDIA_TYPE_BYTES + 1),
                bytes: b"body".to_vec(),
            },
            reference: &Reference::Tag("stable".to_owned()),
            reservation: None,
            journal: false,
            webhook: None,
        },
    );
    assert!(
        matches!(result, Err(ServeError::Transport(message)) if message.contains("over the 65535-byte record limit"))
    );
    assert_eq!(
        (
            crate::store::get_manifest(&meta, "sha256:overflow").unwrap(),
            crate::store::get_tag(&meta, "store", "app", "stable").unwrap(),
        ),
        (None, None)
    );
}

#[test]
fn test_manifest_tag_replacement_commits_a_new_allocation() {
    let (_dir, meta) = store();
    let manifest = Manifest {
        media_type: "application/vnd.oci.image.manifest.v1+json".to_owned(),
        bytes: b"{}".to_vec(),
    };
    let reference = Reference::Tag("stable".to_owned());

    for (digest, created_at_unix) in [("sha256:a", 1), ("sha256:b", 2)] {
        let reservation = meta
            .reserve_quota(
                quota_reservation("store", "app", Some("stable"), digest, 4, created_at_unix),
                QuotaLimits::default(),
            )
            .unwrap();
        publish_manifest(
            &meta,
            ManifestCommit {
                index: "store",
                repo: "app",
                canonical: digest,
                manifest: &manifest,
                reference: &reference,
                reservation: Some(reservation.clone()),
                journal: false,
                webhook: None,
            },
        )
        .unwrap();
        assert_eq!(
            meta.quota_reservation(reservation.id).unwrap().unwrap().state,
            QuotaReservationState::Committed
        );
    }

    assert_eq!(
        (
            meta.quota_usage("store").unwrap().accounted_bytes.committed,
            meta.quota_resource_usage("store", "app").unwrap().groups.committed,
        ),
        (8, 1)
    );
}
