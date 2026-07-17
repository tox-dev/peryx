use std::sync::{Arc, Barrier};

use crate::meta::{
    AccountingClass, MetaStore, NewQuotaReservation, QuotaError, QuotaLimit, QuotaLimits, QuotaProjectUsage,
    QuotaReservationState, QuotaUsage, QuotaValue,
};

use super::store;

#[test]
fn test_quota_rejects_invalid_identities() {
    let (_dir, meta) = store();
    let too_long = "x".repeat(513);
    for (case, invalid, expected) in [
        (
            "empty repository",
            NewQuotaReservation {
                repository: "",
                ..request("package", "1.0", "sha256:first", 7)
            },
            "repository must not be empty",
        ),
        (
            "empty project",
            request("", "1.0", "sha256:first", 7),
            "project must not be empty",
        ),
        (
            "empty digest",
            request("package", "1.0", "", 7),
            "digest must not be empty",
        ),
        (
            "version without project",
            NewQuotaReservation {
                project: None,
                ..request("package", "1.0", "sha256:first", 7)
            },
            "version requires a project",
        ),
        (
            "long project",
            request(&too_long, "1.0", "sha256:first", 7),
            "project exceeds 512 bytes",
        ),
    ] {
        assert_eq!(
            (
                case,
                meta.reserve_quota(invalid, QuotaLimits::default())
                    .unwrap_err()
                    .to_string(),
            ),
            (case, expected.to_owned())
        );
    }
}

#[test]
fn test_quota_allows_content_without_project_or_version_counts() {
    let (_dir, meta) = store();
    let reservation = meta
        .reserve_quota(
            NewQuotaReservation {
                repository: "private",
                project: None,
                version: None,
                digest: "sha256:first",
                bytes: 7,
                class: AccountingClass::Generated,
                created_at_unix: 10,
            },
            QuotaLimits {
                max_projects: Some(0),
                max_versions_per_project: Some(0),
                ..QuotaLimits::default()
            },
        )
        .unwrap();
    meta.commit_quota_reservation(reservation.id).unwrap();

    assert_eq!(
        meta.quota_usage("private").unwrap(),
        crate::meta::QuotaUsage {
            file_bytes: QuotaValue {
                committed: 7,
                reserved: 0,
            },
            accounted_bytes: QuotaValue {
                committed: 7,
                reserved: 0,
            },
            projects: QuotaValue::default(),
        }
    );
}

#[test]
fn test_quota_allows_project_without_version_count() {
    let (_dir, meta) = store();
    let reservation = meta
        .reserve_quota(
            NewQuotaReservation {
                version: None,
                ..request("package", "1.0", "sha256:first", 7)
            },
            QuotaLimits {
                max_projects: Some(1),
                max_versions_per_project: Some(0),
                ..QuotaLimits::default()
            },
        )
        .unwrap();
    meta.commit_quota_reservation(reservation.id).unwrap();

    assert_eq!(
        (
            meta.quota_usage("private").unwrap().projects,
            meta.quota_project_usage("private", "package").unwrap().versions,
        ),
        (
            QuotaValue {
                committed: 1,
                reserved: 0,
            },
            QuotaValue::default(),
        )
    );
}

#[test]
fn test_quota_reserve_commit_release_updates_counters() {
    let (_dir, meta) = store();
    let reservation = meta
        .reserve_quota(request("package", "1.0", "sha256:first", 7), QuotaLimits::default())
        .unwrap();

    assert_eq!(
        (
            meta.quota_usage("private").unwrap(),
            meta.quota_project_usage("private", "package").unwrap(),
        ),
        (
            QuotaUsage {
                file_bytes: QuotaValue {
                    committed: 0,
                    reserved: 7,
                },
                accounted_bytes: QuotaValue {
                    committed: 0,
                    reserved: 7,
                },
                projects: QuotaValue {
                    committed: 0,
                    reserved: 1,
                },
            },
            QuotaProjectUsage {
                versions: QuotaValue {
                    committed: 0,
                    reserved: 1,
                },
            },
        )
    );

    assert!(meta.commit_quota_reservation(reservation.id).unwrap());
    assert_eq!(
        (
            meta.quota_usage("private").unwrap(),
            meta.quota_project_usage("private", "package").unwrap(),
        ),
        (
            QuotaUsage {
                file_bytes: QuotaValue {
                    committed: 7,
                    reserved: 0,
                },
                accounted_bytes: QuotaValue {
                    committed: 7,
                    reserved: 0,
                },
                projects: QuotaValue {
                    committed: 1,
                    reserved: 0,
                },
            },
            QuotaProjectUsage {
                versions: QuotaValue {
                    committed: 1,
                    reserved: 0,
                },
            },
        )
    );

    assert!(meta.release_quota_reservation(reservation.id).unwrap());
    assert_eq!(meta.quota_usage("private").unwrap(), QuotaUsage::default());
    assert_eq!(
        meta.quota_project_usage("private", "package").unwrap(),
        QuotaProjectUsage::default()
    );
}

#[test]
fn test_quota_duplicate_commit_and_release_have_no_effect() {
    let (_dir, meta) = store();
    let id = meta
        .reserve_quota(request("package", "1.0", "sha256:first", 7), QuotaLimits::default())
        .unwrap()
        .id;

    assert_eq!(
        (
            meta.commit_quota_reservation(id).unwrap(),
            meta.commit_quota_reservation(id).unwrap(),
            meta.release_quota_reservation(id).unwrap(),
            meta.release_quota_reservation(id).unwrap(),
            meta.quota_reservation(id).unwrap(),
            meta.quota_usage("private").unwrap(),
        ),
        (true, false, true, false, None, QuotaUsage::default())
    );
}

#[test]
fn test_quota_commit_after_release_reports_no_reservation() {
    let (_dir, meta) = store();
    let id = meta
        .reserve_quota(request("package", "1.0", "sha256:first", 7), QuotaLimits::default())
        .unwrap()
        .id;
    meta.release_quota_reservation(id).unwrap();

    assert_eq!(
        (
            meta.commit_quota_reservation(id).unwrap(),
            meta.quota_usage("private").unwrap(),
        ),
        (false, QuotaUsage::default())
    );
}

#[test]
fn test_quota_commit_is_atomic_with_driver_metadata() {
    let (_dir, meta) = store();
    let id = meta
        .reserve_quota(request("package", "1.0", "sha256:first", 7), QuotaLimits::default())
        .unwrap()
        .id;

    meta.commit_driver_txn_with_quota(id, |txn| {
        txn.put_local("published/package/1.0", b"sha256:first")?;
        Ok::<_, QuotaError>(((), Vec::new()))
    })
    .unwrap();

    assert_eq!(
        (
            meta.get_driver_value("published/package/1.0").unwrap(),
            meta.quota_usage("private").unwrap().accounted_bytes,
        ),
        (
            Some(b"sha256:first".to_vec()),
            QuotaValue {
                committed: 7,
                reserved: 0,
            },
        )
    );
}

#[test]
fn test_quota_failed_driver_commit_leaves_reservation_pending() {
    let (_dir, meta) = store();
    let id = meta
        .reserve_quota(request("package", "1.0", "sha256:first", 7), QuotaLimits::default())
        .unwrap()
        .id;

    let result = meta.commit_driver_txn_with_quota(id, |txn| {
        txn.put_local("published/package/1.0", b"sha256:first")?;
        Err::<((), Vec<Vec<u8>>), _>(QuotaError::Store(crate::meta::MetaError::DriverPrecondition(
            "failed".to_owned(),
        )))
    });

    assert_eq!(
        (
            result.is_err(),
            meta.get_driver_value("published/package/1.0").unwrap(),
            meta.quota_usage("private").unwrap().accounted_bytes,
        ),
        (
            true,
            None,
            QuotaValue {
                committed: 0,
                reserved: 7,
            },
        )
    );
}

#[test]
fn test_quota_atomic_commit_rejects_used_reservation() {
    let (_dir, meta) = store();
    let id = meta
        .reserve_quota(request("package", "1.0", "sha256:first", 7), QuotaLimits::default())
        .unwrap()
        .id;
    meta.commit_quota_reservation(id).unwrap();

    let result = meta.commit_driver_txn_with_quota(id, |txn| {
        txn.put_local("published/package/1.0", b"sha256:first")?;
        Ok::<_, QuotaError>(((), Vec::new()))
    });

    assert_eq!(
        (
            matches!(result, Err(QuotaError::ReservationUnavailable { id: failed }) if failed == id),
            meta.get_driver_value("published/package/1.0").unwrap(),
        ),
        (true, None)
    );
}

#[test]
fn test_quota_deduplicates_digest_within_repository() {
    let (_dir, meta) = store();
    let first = meta
        .reserve_quota(request("one", "1.0", "sha256:shared", 7), QuotaLimits::default())
        .unwrap();
    let second = meta
        .reserve_quota(
            request("two", "1.0", "sha256:shared", 7),
            QuotaLimits {
                max_accounted_bytes: Some(7),
                ..QuotaLimits::default()
            },
        )
        .unwrap();

    assert_eq!(
        (
            meta.quota_usage("private").unwrap().file_bytes,
            meta.quota_usage("private").unwrap().accounted_bytes,
        ),
        (
            QuotaValue {
                committed: 0,
                reserved: 14,
            },
            QuotaValue {
                committed: 0,
                reserved: 7,
            },
        )
    );

    meta.commit_quota_reservation(first.id).unwrap();
    meta.release_quota_reservation(first.id).unwrap();
    assert_eq!(
        meta.quota_usage("private").unwrap().accounted_bytes,
        QuotaValue {
            committed: 0,
            reserved: 7,
        }
    );
    meta.commit_quota_reservation(second.id).unwrap();
    assert_eq!(
        meta.quota_usage("private").unwrap().accounted_bytes,
        QuotaValue {
            committed: 7,
            reserved: 0,
        }
    );
}

#[test]
fn test_quota_charges_shared_digest_to_each_repository() {
    let (_dir, meta) = store();
    meta.reserve_quota(request("package", "1.0", "sha256:shared", 7), QuotaLimits::default())
        .unwrap();
    meta.reserve_quota(
        NewQuotaReservation {
            repository: "public",
            ..request("package", "1.0", "sha256:shared", 7)
        },
        QuotaLimits::default(),
    )
    .unwrap();

    assert_eq!(
        (
            meta.quota_usage("private").unwrap().accounted_bytes,
            meta.quota_usage("public").unwrap().accounted_bytes,
        ),
        (
            QuotaValue {
                committed: 0,
                reserved: 7,
            },
            QuotaValue {
                committed: 0,
                reserved: 7,
            },
        )
    );
}

#[test]
fn test_quota_releasing_pending_duplicate_preserves_committed_digest() {
    let (_dir, meta) = store();
    let committed = meta
        .reserve_quota(request("one", "1.0", "sha256:shared", 7), QuotaLimits::default())
        .unwrap();
    meta.commit_quota_reservation(committed.id).unwrap();
    let pending = meta
        .reserve_quota(request("two", "1.0", "sha256:shared", 7), QuotaLimits::default())
        .unwrap();

    meta.release_quota_reservation(pending.id).unwrap();

    assert_eq!(
        meta.quota_usage("private").unwrap().accounted_bytes,
        QuotaValue {
            committed: 7,
            reserved: 0,
        }
    );
}

#[test]
fn test_quota_classes_account_shared_digest_without_double_charging_repository() {
    let (_dir, meta) = store();
    let hosted = meta
        .reserve_quota(request("package", "1.0", "sha256:shared", 7), QuotaLimits::default())
        .unwrap();
    let mut trashed = request("package", "1.0", "sha256:shared", 7);
    trashed.class = AccountingClass::Trash;
    let trashed = meta.reserve_quota(trashed, QuotaLimits::default()).unwrap();
    meta.commit_quota_reservation(hosted.id).unwrap();
    meta.commit_quota_reservation(trashed.id).unwrap();
    let usage = meta.quota_usage("private").unwrap();

    assert_eq!((usage.accounted_bytes.committed, usage.file_bytes.committed), (7, 14));
}

#[test]
fn test_quota_accounts_every_content_class() {
    let (_dir, meta) = store();
    let mut reservations = Vec::new();
    for (position, class) in [
        AccountingClass::Hosted,
        AccountingClass::Cached,
        AccountingClass::Generated,
        AccountingClass::Trash,
    ]
    .into_iter()
    .enumerate()
    {
        let identity = format!("item-{position}");
        let mut item = request(&identity, "1.0", &identity, 7);
        item.class = class;
        let reservation = meta.reserve_quota(item, QuotaLimits::default()).unwrap();
        meta.commit_quota_reservation(reservation.id).unwrap();
        reservations.push(meta.quota_reservation(reservation.id).unwrap().unwrap());
    }

    assert_eq!(
        (
            reservations
                .into_iter()
                .map(|reservation| reservation.class)
                .collect::<Vec<_>>(),
            meta.quota_usage("private").unwrap().accounted_bytes,
        ),
        (
            vec![
                AccountingClass::Hosted,
                AccountingClass::Cached,
                AccountingClass::Generated,
                AccountingClass::Trash,
            ],
            QuotaValue {
                committed: 28,
                reserved: 0,
            },
        )
    );
}

#[test]
fn test_quota_audit_records_all_violations() {
    let (_dir, meta) = store();
    let outcome = meta
        .reserve_quota(
            request("package", "1.0", "sha256:first", 7),
            QuotaLimits {
                max_file_bytes: Some(6),
                max_accounted_bytes: Some(6),
                max_projects: Some(0),
                max_versions_per_project: Some(0),
                audit: true,
            },
        )
        .unwrap();

    assert_eq!(
        (
            outcome.violations,
            outcome.state,
            meta.quota_usage("private").unwrap().accounted_bytes.reserved,
        ),
        (
            vec![
                QuotaLimit::FileBytes,
                QuotaLimit::AccountedBytes,
                QuotaLimit::Projects,
                QuotaLimit::VersionsPerProject,
            ],
            QuotaReservationState::Reserved,
            7,
        )
    );
}

#[test]
fn test_quota_enforcement_rejects_without_writes() {
    let (_dir, meta) = store();
    let error = meta
        .reserve_quota(
            request("package", "1.0", "sha256:first", 7),
            QuotaLimits {
                max_accounted_bytes: Some(6),
                ..QuotaLimits::default()
            },
        )
        .unwrap_err();

    assert_eq!(
        (
            matches!(
                error,
                QuotaError::Exceeded {
                    violations
                } if violations == [QuotaLimit::AccountedBytes]
            ),
            meta.quota_usage("private").unwrap(),
        ),
        (true, QuotaUsage::default())
    );
}

#[test]
fn test_quota_counter_overflow_rejects_without_writes() {
    let (_dir, meta) = store();
    meta.reserve_quota(
        request("package", "1.0", "sha256:first", u64::MAX),
        QuotaLimits::default(),
    )
    .unwrap();

    assert_eq!(
        (
            matches!(
                meta.reserve_quota(request("package", "1.0", "sha256:second", 1), QuotaLimits::default()),
                Err(QuotaError::CounterOverflow)
            ),
            meta.quota_usage("private").unwrap().file_bytes,
        ),
        (
            true,
            QuotaValue {
                committed: 0,
                reserved: u64::MAX,
            },
        )
    );
}

#[test]
fn test_quota_parallel_reservations_admit_only_capacity_that_fits() {
    let (_dir, meta) = store();
    let meta = Arc::new(meta);
    let barrier = Arc::new(Barrier::new(3));
    let threads = ["first", "second"].map(|digest| {
        let meta = Arc::clone(&meta);
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            meta.reserve_quota(
                request(digest, "1.0", digest, 7),
                QuotaLimits {
                    max_accounted_bytes: Some(10),
                    ..QuotaLimits::default()
                },
            )
        })
    });
    barrier.wait();
    let results = threads.map(|thread| thread.join().unwrap());

    assert_eq!(
        (
            results.iter().filter(|result| result.is_ok()).count(),
            results
                .iter()
                .filter(|result| matches!(result, Err(QuotaError::Exceeded { .. })))
                .count(),
            meta.quota_usage("private").unwrap().accounted_bytes.reserved,
        ),
        (1, 1, 7)
    );
}

#[test]
fn test_quota_repair_is_bounded_and_preserves_committed_allocations() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let meta = MetaStore::open(&path).unwrap();
    let committed = meta
        .reserve_quota(request("keep", "1.0", "sha256:keep", 5), QuotaLimits::default())
        .unwrap();
    meta.commit_quota_reservation(committed.id).unwrap();
    for digest in ["sha256:first", "sha256:second"] {
        meta.reserve_quota(request(digest, "1.0", digest, 7), QuotaLimits::default())
            .unwrap();
    }
    drop(meta);

    let meta = MetaStore::open(&path).unwrap();
    assert_eq!(
        (
            meta.repair_abandoned_quota_reservations(1).unwrap(),
            meta.repair_abandoned_quota_reservations(1).unwrap(),
            meta.quota_usage("private").unwrap().accounted_bytes,
        ),
        (
            crate::meta::QuotaRepairReport {
                released: 1,
                remaining: true,
            },
            crate::meta::QuotaRepairReport {
                released: 1,
                remaining: false,
            },
            QuotaValue {
                committed: 5,
                reserved: 0,
            },
        )
    );
}

#[test]
fn test_quota_repair_zero_limit_changes_nothing() {
    let (_dir, meta) = store();
    meta.reserve_quota(request("package", "1.0", "sha256:first", 7), QuotaLimits::default())
        .unwrap();

    assert_eq!(
        (
            meta.repair_abandoned_quota_reservations(0).unwrap(),
            meta.quota_usage("private").unwrap().accounted_bytes.reserved,
        ),
        (crate::meta::QuotaRepairReport::default(), 7)
    );
}

#[test]
fn test_quota_rejects_digest_size_mismatch() {
    let (_dir, meta) = store();
    meta.reserve_quota(request("one", "1.0", "sha256:shared", 7), QuotaLimits::default())
        .unwrap();

    assert!(matches!(
        meta.reserve_quota(request("two", "1.0", "sha256:shared", 8), QuotaLimits::default()),
        Err(QuotaError::DigestSize {
            actual: 7,
            requested: 8,
            ..
        })
    ));
}

#[test]
fn test_quota_tables_initialize_in_an_existing_database() {
    const OLD_TABLE: redb::TableDefinition<&str, &str> = redb::TableDefinition::new("old_metadata");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let db = redb::Database::create(&path).unwrap();
    let txn = db.begin_write().unwrap();
    txn.open_table(OLD_TABLE).unwrap();
    txn.commit().unwrap();
    drop(db);

    let meta = MetaStore::open(&path).unwrap();
    meta.reserve_quota(request("package", "1.0", "sha256:first", 7), QuotaLimits::default())
        .unwrap();

    assert_eq!(meta.quota_usage("private").unwrap().accounted_bytes.reserved, 7);
}

fn request<'a>(project: &'a str, version: &'a str, digest: &'a str, bytes: u64) -> NewQuotaReservation<'a> {
    NewQuotaReservation {
        repository: "private",
        project: Some(project),
        version: Some(version),
        digest,
        bytes,
        class: AccountingClass::Hosted,
        created_at_unix: 10,
    }
}
