use peryx_storage::meta::{AccountingClass, MetaStore, QuotaLimit, QuotaLimits};

use super::{ReserveOutcome, describe, finalize, quota_reservation, reserve};
use crate::registry::ServeError;

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, meta)
}

#[test]
fn test_reserve_admits_within_the_limit() {
    let (_dir, meta) = store();
    let request = quota_reservation("store", "app", None, "sha256:a", 4, AccountingClass::Hosted, 1);
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
    let request = quota_reservation("store", "app", None, "sha256:a", 9, AccountingClass::Hosted, 1);
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
    let request = quota_reservation(&long, "app", None, "sha256:a", 1, AccountingClass::Hosted, 1);
    assert!(reserve(&meta, request, QuotaLimits::default()).is_err());
}

#[test]
fn test_describe_names_each_crossed_counter() {
    assert_eq!(
        describe(&[
            QuotaLimit::ArtifactBytes,
            QuotaLimit::AccountedBytes,
            QuotaLimit::Resources,
            QuotaLimit::GroupsPerResource,
        ]),
        "file size, repository bytes, repository projects, project versions"
    );
}

#[test]
fn test_finalize_releases_the_reservation_after_a_driver_failure() {
    let (_dir, meta) = store();
    let request = quota_reservation("store", "app", None, "sha256:a", 4, AccountingClass::Hosted, 1);
    let record = meta.reserve_quota(request, QuotaLimits::default()).unwrap();
    let id = record.id;

    let result = finalize(&meta, Some(record), None, |_txn| {
        Err::<((), Vec<Vec<u8>>), _>(ServeError::Transport("driver write failed".to_owned()))
    });

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
