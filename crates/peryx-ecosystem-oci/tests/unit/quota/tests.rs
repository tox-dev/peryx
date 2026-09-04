use peryx_storage::meta::{MetaStore, QuotaLimit, QuotaLimits, QuotaReservationState, QuotaUsage};

use super::{
    ManifestCheckpoint, ManifestCommit, ManifestOperation, ReserveOutcome, commit_blob_membership, finalize,
    publish_manifest, quota_reservation, release_blob_membership, reserve,
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
                referrer: None,
                reservation: Some(reservation),
                journal: false,
                webhook: None,
                operation: None,
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
            referrer: None,
            reservation: None,
            journal: false,
            webhook: None,
            operation: None,
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
                referrer: None,
                reservation: Some(reservation.clone()),
                journal: false,
                webhook: None,
                operation: None,
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

/// A stored checkpoint that cannot be decoded has to fail the replay it was read for. Treating it as
/// absent would let a request that already committed run a second time.
///
/// The round trip is the control: without it a `decode` that rejected everything would pass.
#[test]
fn test_manifest_checkpoint_reports_a_record_it_cannot_decode() {
    let checkpoint = ManifestCheckpoint {
        reference: "sha256:aa".to_owned(),
        epoch: 3,
        serial: 9,
    };

    assert_eq!(ManifestCheckpoint::decode(&checkpoint.encode()).unwrap(), checkpoint);
    assert!(matches!(
        ManifestCheckpoint::decode(b"not a checkpoint"),
        Err(ServeError::Transport(_))
    ));
}

/// A publish that cannot record its idempotency checkpoint must not leave the manifest behind. The
/// checkpoint is what a retry reads to decide the work is already done, so a manifest committed
/// without one is published again on the next attempt as though it had never landed.
///
/// Each step gets its own backend, because a publish mutates and the next injection point would
/// otherwise run against whatever the previous one left. The fault is armed after the store opens
/// so the count applies to the publish rather than to creating the tables.
#[test]
fn test_publish_manifest_never_commits_without_its_checkpoint() {
    let manifest = Manifest {
        media_type: "application/vnd.oci.image.manifest.v1+json".to_owned(),
        bytes: b"{}".to_vec(),
    };
    let reference = Reference::Tag("stable".to_owned());
    // The control: with nothing injected the publish lands and the manifest is readable. Without it
    // the sweep below would pass against a publish that always failed.
    let (pages, fault) = peryx_test_support::fault::backend();
    let meta = MetaStore::open_backend(peryx_test_support::fault::faulted(&pages, &fault)).unwrap();
    meta.claim_operation("op-1", None, 100).unwrap();
    publish_manifest(&meta, checkpoint_commit(&manifest, &reference)).unwrap();
    drop(meta);
    let meta = MetaStore::reopen_backend(peryx_test_support::fault::faulted(&pages, &fault)).unwrap();
    assert!(meta.get_driver_value("oci\u{0}m\u{0}sha256:a").unwrap().is_some());
    drop(meta);

    let mut failed = 0_u32;
    for fail_after in 0..192 {
        let (pages, fault) = peryx_test_support::fault::backend();
        let meta = MetaStore::open_backend(peryx_test_support::fault::faulted(&pages, &fault)).unwrap();
        // The checkpoint refuses an operation nobody claimed, so without this every step fails for
        // that reason instead of the injected one and the sweep proves nothing.
        meta.claim_operation("op-1", None, 100).unwrap();
        fault.arm(fail_after);
        let published = publish_manifest(&meta, checkpoint_commit(&manifest, &reference));
        fault.disable();
        drop(meta);

        let meta = MetaStore::reopen_backend(peryx_test_support::fault::faulted(&pages, &fault)).unwrap();
        let stored = meta.get_driver_value("oci\u{0}m\u{0}sha256:a").unwrap().is_some();
        // Empty while pending, so a recorded response is what says the checkpoint landed. Presence
        // of the record alone would only prove the claim above ran.
        let checkpointed = meta
            .operation_outcome("op-1")
            .unwrap()
            .is_some_and(|record| !record.response.is_empty());
        failed += u32::from(published.is_err());
        assert_eq!(
            stored,
            checkpointed,
            "injecting after {fail_after} reads left manifest={stored} and checkpoint={checkpointed} \
             disagreeing, published_ok={}",
            published.is_ok()
        );
    }

    assert!(failed > 0, "no injection point reached the publish");
}

fn checkpoint_commit<'a>(manifest: &'a Manifest, reference: &'a Reference) -> ManifestCommit<'a> {
    ManifestCommit {
        index: "store",
        repo: "app",
        canonical: "sha256:a",
        manifest,
        reference,
        referrer: None,
        reservation: None,
        journal: true,
        webhook: None,
        operation: Some(ManifestOperation {
            id: "op-1",
            reference: "stable",
            epoch: 1,
            now: 100,
        }),
    }
}
