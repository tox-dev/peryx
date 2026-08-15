use std::sync::{Arc, Barrier};

use rstest::{fixture, rstest};

use crate::meta::{
    AccountingClass, MetaStore, NewQuotaReservation, QuotaAllocation, QuotaError, QuotaLimit, QuotaLimits,
    QuotaReservationState, QuotaResourceUsage, QuotaUsage, QuotaValue,
};

use super::store;

type ArtifactByteQuota = (tempfile::TempDir, MetaStore, QuotaLimits);

enum QuotaTransition {
    Reserved,
    Committed,
    Released,
}

enum ReservationResult {
    Admitted,
    Exceeded,
}

#[fixture]
fn artifact_byte_quota() -> ArtifactByteQuota {
    let (dir, meta) = store();
    (
        dir,
        meta,
        QuotaLimits {
            max_artifact_bytes: Some(10),
            ..QuotaLimits::default()
        },
    )
}

#[test]
fn test_quota_rejects_invalid_identities() {
    let (_dir, meta) = store();
    let too_long = "x".repeat(513);
    for (case, invalid, expected) in [
        (
            "empty repository",
            NewQuotaReservation {
                repository: "",
                ..request("resource-a", "group-a", "sha256:first", 7)
            },
            "repository must not be empty",
        ),
        (
            "empty resource",
            request("", "group-a", "sha256:first", 7),
            "resource must not be empty",
        ),
        (
            "empty digest",
            request("resource-a", "group-a", "", 7),
            "digest must not be empty",
        ),
        (
            "group without resource",
            NewQuotaReservation {
                resource: None,
                ..request("resource-a", "group-a", "sha256:first", 7)
            },
            "group requires a resource",
        ),
        (
            "long resource",
            request(&too_long, "group-a", "sha256:first", 7),
            "resource exceeds 512 bytes",
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
fn test_quota_allows_content_without_resource_or_group_counts() {
    let (_dir, meta) = store();
    let reservation = meta
        .reserve_quota(
            NewQuotaReservation {
                repository: "private",
                resource: None,
                group: None,
                digest: "sha256:first",
                bytes: 7,
                class: AccountingClass::Generated,
                created_at_unix: 10,
            },
            QuotaLimits {
                max_resources: Some(0),
                max_groups_per_resource: Some(0),
                ..QuotaLimits::default()
            },
        )
        .unwrap();
    meta.commit_quota_reservation(reservation.id).unwrap();

    assert_eq!(
        meta.quota_usage("private").unwrap(),
        crate::meta::QuotaUsage {
            artifact_bytes: QuotaValue {
                committed: 7,
                reserved: 0,
            },
            accounted_bytes: QuotaValue {
                committed: 7,
                reserved: 0,
            },
            resources: QuotaValue::default(),
        }
    );
}

#[test]
fn test_quota_allows_resource_without_group_count() {
    let (_dir, meta) = store();
    let reservation = meta
        .reserve_quota(
            NewQuotaReservation {
                group: None,
                ..request("resource-a", "group-a", "sha256:first", 7)
            },
            QuotaLimits {
                max_resources: Some(1),
                max_groups_per_resource: Some(0),
                ..QuotaLimits::default()
            },
        )
        .unwrap();
    meta.commit_quota_reservation(reservation.id).unwrap();

    assert_eq!(
        (
            meta.quota_usage("private").unwrap().resources,
            meta.quota_resource_usage("private", "resource-a").unwrap().groups,
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
        .reserve_quota(
            request("resource-a", "group-a", "sha256:first", 7),
            QuotaLimits::default(),
        )
        .unwrap();

    assert_eq!(
        (
            meta.quota_usage("private").unwrap(),
            meta.quota_resource_usage("private", "resource-a").unwrap(),
        ),
        (
            QuotaUsage {
                artifact_bytes: QuotaValue {
                    committed: 0,
                    reserved: 7,
                },
                accounted_bytes: QuotaValue {
                    committed: 0,
                    reserved: 7,
                },
                resources: QuotaValue {
                    committed: 0,
                    reserved: 1,
                },
            },
            QuotaResourceUsage {
                artifact_bytes: QuotaValue::default(),
                groups: QuotaValue {
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
            meta.quota_resource_usage("private", "resource-a").unwrap(),
        ),
        (
            QuotaUsage {
                artifact_bytes: QuotaValue {
                    committed: 7,
                    reserved: 0,
                },
                accounted_bytes: QuotaValue {
                    committed: 7,
                    reserved: 0,
                },
                resources: QuotaValue {
                    committed: 1,
                    reserved: 0,
                },
            },
            QuotaResourceUsage {
                artifact_bytes: QuotaValue::default(),
                groups: QuotaValue {
                    committed: 1,
                    reserved: 0,
                },
            },
        )
    );

    assert!(meta.release_quota_reservation(reservation.id).unwrap());
    assert_eq!(meta.quota_usage("private").unwrap(), QuotaUsage::default());
    assert_eq!(
        meta.quota_resource_usage("private", "resource-a").unwrap(),
        QuotaResourceUsage::default()
    );
}

#[test]
fn test_quota_duplicate_commit_and_release_have_no_effect() {
    let (_dir, meta) = store();
    let id = meta
        .reserve_resource_quota(request("resource-a", "group-a", "sha256:first", 7), 7, false)
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
        .reserve_quota(
            request("resource-a", "group-a", "sha256:first", 7),
            QuotaLimits::default(),
        )
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
        .reserve_quota(
            request("resource-a", "group-a", "sha256:first", 7),
            QuotaLimits::default(),
        )
        .unwrap()
        .id;

    meta.commit_driver_txn_with_quota(id, |txn| {
        txn.put_local("published/resource/group", b"sha256:first")?;
        Ok::<_, QuotaError>(((), Vec::new()))
    })
    .unwrap();

    assert_eq!(
        (
            meta.get_driver_value("published/resource/group").unwrap(),
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
fn test_quota_conditional_commit_releases_a_skipped_write() {
    let (_dir, meta) = store();
    let id = meta
        .reserve_resource_quota(request("resource-a", "group-a", "sha256:first", 7), 7, false)
        .unwrap()
        .id;

    let stored = meta
        .commit_driver_txn_with_quota_if(id, |stored| *stored, |_txn| Ok::<_, QuotaError>((false, Vec::new())))
        .unwrap();

    assert!(!stored);
    assert_eq!(meta.quota_usage("private").unwrap(), QuotaUsage::default());
    assert_eq!(meta.quota_reservation(id).unwrap(), None);
}

#[test]
fn test_quota_conditional_commit_publishes_an_accepted_write() {
    let (_dir, meta) = store();
    let id = meta
        .reserve_resource_quota(request("resource-a", "group-a", "sha256:first", 7), 7, false)
        .unwrap()
        .id;

    let stored = meta
        .commit_driver_txn_with_quota_if(
            id,
            |stored| *stored,
            |txn| {
                txn.put_local("published/resource/group", b"sha256:first")?;
                Ok::<_, QuotaError>((true, Vec::new()))
            },
        )
        .unwrap();

    assert!(stored);
    assert_eq!(
        (
            meta.get_driver_value("published/resource/group").unwrap(),
            meta.quota_resource_usage("private", "resource-a")
                .unwrap()
                .artifact_bytes,
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
fn test_quota_conditional_skip_rejects_a_committed_reservation() {
    let (_dir, meta) = store();
    let id = meta
        .reserve_resource_quota(request("resource-a", "group-a", "sha256:first", 7), 7, false)
        .unwrap()
        .id;
    meta.commit_quota_reservation(id).unwrap();

    let result = meta.commit_driver_txn_with_quota_if(
        id,
        |stored| *stored,
        |txn| {
            txn.put_local("published/resource/group", b"sha256:first")?;
            Ok::<_, QuotaError>((false, Vec::new()))
        },
    );

    assert!(matches!(result, Err(QuotaError::ReservationUnavailable { .. })));
    assert!(meta.get_driver_value("published/resource/group").unwrap().is_none());
    assert_eq!(
        meta.quota_resource_usage("private", "resource-a")
            .unwrap()
            .artifact_bytes,
        QuotaValue {
            committed: 7,
            reserved: 0,
        }
    );
}

#[rstest]
#[case::commit(true, false)]
#[case::release(false, false)]
#[case::unavailable(false, true)]
fn test_quota_conditional_commit_returns_its_journal(#[case] commit: bool, #[case] precommitted: bool) {
    let (_dir, meta) = store();
    let id = meta
        .reserve_resource_quota(request("resource-a", "group-a", "sha256:first", 7), 7, false)
        .unwrap()
        .id;
    if precommitted {
        assert!(meta.commit_quota_reservation(id).unwrap());
    }

    let result = meta.commit_driver_txn_with_quota_if_commit(
        id,
        |_| commit,
        |txn| {
            txn.put_local("published/resource/group", b"sha256:first")?;
            Ok::<_, QuotaError>((commit, vec![b"published".to_vec()]))
        },
    );

    if precommitted {
        assert!(matches!(result, Err(QuotaError::ReservationUnavailable { .. })));
        assert_eq!(meta.get_driver_value("published/resource/group").unwrap(), None);
        assert_eq!(
            meta.quota_resource_usage("private", "resource-a")
                .unwrap()
                .artifact_bytes,
            QuotaValue {
                committed: 7,
                reserved: 0,
            }
        );
        return;
    }

    let result = result.unwrap();
    assert_eq!((result.value, result.journal.unwrap().serial()), (commit, 1));
    assert_eq!(
        meta.get_driver_value("published/resource/group").unwrap(),
        Some(b"sha256:first".to_vec())
    );
    assert_eq!(
        meta.quota_resource_usage("private", "resource-a")
            .unwrap()
            .artifact_bytes,
        if commit {
            QuotaValue {
                committed: 7,
                reserved: 0,
            }
        } else {
            QuotaValue::default()
        }
    );
}

#[test]
fn test_quota_failed_driver_commit_leaves_reservation_pending() {
    let (_dir, meta) = store();
    let id = meta
        .reserve_quota(
            request("resource-a", "group-a", "sha256:first", 7),
            QuotaLimits::default(),
        )
        .unwrap()
        .id;

    let result = meta.commit_driver_txn_with_quota(id, |txn| {
        txn.put_local("published/resource/group", b"sha256:first")?;
        Err::<((), Vec<Vec<u8>>), _>(QuotaError::Store(crate::meta::MetaError::DriverPrecondition(
            "failed".to_owned(),
        )))
    });

    assert_eq!(
        (
            result.is_err(),
            meta.get_driver_value("published/resource/group").unwrap(),
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
        .reserve_quota(
            request("resource-a", "group-a", "sha256:first", 7),
            QuotaLimits::default(),
        )
        .unwrap()
        .id;
    meta.commit_quota_reservation(id).unwrap();

    let result = meta.commit_driver_txn_with_quota(id, |txn| {
        txn.put_local("published/resource/group", b"sha256:first")?;
        Ok::<_, QuotaError>(((), Vec::new()))
    });

    assert_eq!(
        (
            matches!(result, Err(QuotaError::ReservationUnavailable { id: failed }) if failed == id),
            meta.get_driver_value("published/resource/group").unwrap(),
        ),
        (true, None)
    );
}

#[test]
fn test_quota_deduplicates_digest_within_repository() {
    let (_dir, meta) = store();
    let first = meta
        .reserve_quota(request("one", "group-a", "sha256:shared", 7), QuotaLimits::default())
        .unwrap();
    let second = meta
        .reserve_quota(
            request("two", "group-a", "sha256:shared", 7),
            QuotaLimits {
                max_accounted_bytes: Some(7),
                ..QuotaLimits::default()
            },
        )
        .unwrap();

    assert_eq!(
        (
            meta.quota_usage("private").unwrap().artifact_bytes,
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
    meta.reserve_quota(
        request("resource-a", "group-a", "sha256:shared", 7),
        QuotaLimits::default(),
    )
    .unwrap();
    meta.reserve_quota(
        NewQuotaReservation {
            repository: "public",
            ..request("resource-a", "group-a", "sha256:shared", 7)
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
        .reserve_quota(request("one", "group-a", "sha256:shared", 7), QuotaLimits::default())
        .unwrap();
    meta.commit_quota_reservation(committed.id).unwrap();
    let pending = meta
        .reserve_quota(request("two", "group-a", "sha256:shared", 7), QuotaLimits::default())
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
        .reserve_quota(
            request("resource-a", "group-a", "sha256:shared", 7),
            QuotaLimits::default(),
        )
        .unwrap();
    let mut trashed = request("resource-a", "group-a", "sha256:shared", 7);
    trashed.class = AccountingClass::Trash;
    let trashed = meta.reserve_quota(trashed, QuotaLimits::default()).unwrap();
    meta.commit_quota_reservation(hosted.id).unwrap();
    meta.commit_quota_reservation(trashed.id).unwrap();
    let usage = meta.quota_usage("private").unwrap();

    assert_eq!(
        (usage.accounted_bytes.committed, usage.artifact_bytes.committed),
        (7, 14)
    );
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
        let mut item = request(&identity, "group-a", &identity, 7);
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
            request("resource-a", "group-a", "sha256:first", 7),
            QuotaLimits {
                max_artifact_bytes: Some(6),
                max_accounted_bytes: Some(6),
                max_resources: Some(0),
                max_groups_per_resource: Some(0),
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
                QuotaLimit::ArtifactBytes,
                QuotaLimit::AccountedBytes,
                QuotaLimit::Resources,
                QuotaLimit::GroupsPerResource,
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
            request("resource-a", "group-a", "sha256:first", 7),
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
fn test_quota_resource_artifact_bytes_reject_only_the_exhausted_resource() {
    let (_dir, meta) = store();
    let first = meta
        .reserve_resource_quota(request("resource-a", "group-a", "sha256:first", 7), 10, false)
        .unwrap();
    meta.commit_quota_reservation(first.id).unwrap();
    assert!(matches!(
        meta.reserve_resource_quota(request("resource-a", "group-b", "sha256:second", 4), 10, false),
        Err(QuotaError::ResourceExceeded { total: 11 })
    ));
    let other = meta
        .reserve_resource_quota(request("other", "group-a", "sha256:other", 4), 10, false)
        .unwrap();

    assert_eq!(
        (
            meta.quota_resource_usage("private", "resource-a")
                .unwrap()
                .artifact_bytes,
            meta.quota_resource_usage("private", "other").unwrap().artifact_bytes,
            other.state,
        ),
        (
            QuotaValue {
                committed: 7,
                reserved: 0,
            },
            QuotaValue {
                committed: 0,
                reserved: 4,
            },
            QuotaReservationState::Reserved,
        )
    );
}

#[test]
fn test_quota_resource_artifact_bytes_reject_after_the_limit_is_lowered() {
    let (_dir, meta) = store();
    let reservation = meta
        .reserve_resource_quota(request("resource-a", "group-a", "sha256:first", 7), 8, false)
        .unwrap();
    meta.commit_quota_reservation(reservation.id).unwrap();

    let result = meta.reserve_resource_quota(request("resource-a", "group-b", "sha256:second", 1), 6, false);

    assert!(matches!(result, Err(QuotaError::ResourceExceeded { total: 8 })));
    assert_eq!(
        meta.quota_resource_usage("private", "resource-a")
            .unwrap()
            .artifact_bytes,
        QuotaValue {
            committed: 7,
            reserved: 0,
        }
    );
}

#[test]
fn test_quota_resource_artifact_bytes_require_a_resource_identity() {
    let (_dir, meta) = store();
    let request = NewQuotaReservation {
        resource: None,
        group: None,
        ..request("resource-a", "group-a", "sha256:first", 7)
    };

    assert_eq!(
        meta.reserve_resource_quota(request, 10, false).unwrap_err().to_string(),
        "resource must not be empty"
    );
}

#[test]
fn test_quota_parallel_resource_reservations_share_available_bytes() {
    let (_dir, meta) = store();
    let meta = Arc::new(meta);
    let barrier = Arc::new(Barrier::new(3));
    let threads = ["first", "second"].map(|digest| {
        let meta = Arc::clone(&meta);
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            meta.reserve_resource_quota(request("resource-a", digest, digest, 7), 10, false)
        })
    });
    barrier.wait();
    let results = threads.map(|thread| thread.join().unwrap());

    assert_eq!(
        (
            results.iter().filter(|result| result.is_ok()).count(),
            results
                .iter()
                .filter(|result| matches!(result, Err(QuotaError::ResourceExceeded { .. })))
                .count(),
            meta.quota_resource_usage("private", "resource-a")
                .unwrap()
                .artifact_bytes
                .reserved,
        ),
        (1, 1, 7)
    );
}

#[test]
fn test_quota_counter_overflow_rejects_without_writes() {
    let (_dir, meta) = store();
    meta.reserve_quota(
        request("resource-a", "group-a", "sha256:first", u64::MAX),
        QuotaLimits::default(),
    )
    .unwrap();

    assert!(matches!(
        meta.reserve_quota(
            request("resource-a", "group-a", "sha256:second", 1),
            QuotaLimits::default()
        ),
        Err(QuotaError::CounterOverflow)
    ));
    assert_eq!(
        meta.quota_usage("private").unwrap().artifact_bytes,
        QuotaValue {
            committed: 0,
            reserved: u64::MAX,
        }
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
                request(digest, "group-a", digest, 7),
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

#[rstest]
#[case::reserved_to_limit(
    QuotaTransition::Reserved,
    3,
    ReservationResult::Admitted,
    QuotaValue { committed: 0, reserved: 10 }
)]
#[case::reserved_over_limit(
    QuotaTransition::Reserved,
    4,
    ReservationResult::Exceeded,
    QuotaValue { committed: 0, reserved: 7 }
)]
#[case::committed_over_limit(
    QuotaTransition::Committed,
    4,
    ReservationResult::Exceeded,
    QuotaValue { committed: 7, reserved: 0 }
)]
#[case::released_capacity(
    QuotaTransition::Released,
    7,
    ReservationResult::Admitted,
    QuotaValue { committed: 0, reserved: 10 }
)]
fn test_quota_artifact_bytes_follow_reservation_transitions(
    artifact_byte_quota: ArtifactByteQuota,
    #[case] transition: QuotaTransition,
    #[case] second_size: u64,
    #[case] expected_result: ReservationResult,
    #[case] expected_usage: QuotaValue,
) {
    let (_dir, meta, limits) = artifact_byte_quota;
    let first = meta
        .reserve_quota(request("resource-a", "group-a", "sha256:first", 7), limits)
        .unwrap();
    match transition {
        QuotaTransition::Reserved => {}
        QuotaTransition::Committed => {
            meta.commit_quota_reservation(first.id).unwrap();
        }
        QuotaTransition::Released => {
            meta.reserve_quota(request("resource-a", "group-b", "sha256:second", 3), limits)
                .unwrap();
            meta.release_quota_reservation(first.id).unwrap();
        }
    }

    let result = meta.reserve_quota(
        request("resource-a", "group-c", "sha256:candidate", second_size),
        limits,
    );
    match expected_result {
        ReservationResult::Admitted => assert_eq!(result.unwrap().state, QuotaReservationState::Reserved),
        ReservationResult::Exceeded => assert!(
            matches!(result, Err(QuotaError::Exceeded { violations }) if violations == [QuotaLimit::ArtifactBytes])
        ),
    }
    assert_eq!(meta.quota_usage("private").unwrap().artifact_bytes, expected_usage);
}

#[rstest]
fn test_quota_parallel_reservations_admit_only_artifact_bytes_that_fit(artifact_byte_quota: ArtifactByteQuota) {
    let (_dir, meta, limits) = artifact_byte_quota;
    let meta = Arc::new(meta);
    let barrier = Arc::new(Barrier::new(3));
    let threads = ["first", "second"].map(|digest| {
        let meta = Arc::clone(&meta);
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            meta.reserve_quota(request(digest, "group-a", digest, 7), limits)
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
            meta.quota_usage("private").unwrap().artifact_bytes.reserved,
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
        .reserve_quota(request("keep", "group-a", "sha256:keep", 5), QuotaLimits::default())
        .unwrap();
    meta.commit_quota_reservation(committed.id).unwrap();
    for digest in ["sha256:first", "sha256:second"] {
        meta.reserve_quota(request(digest, "group-a", digest, 7), QuotaLimits::default())
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
    meta.reserve_quota(
        request("resource-a", "group-a", "sha256:first", 7),
        QuotaLimits::default(),
    )
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
    meta.reserve_quota(request("one", "group-a", "sha256:shared", 7), QuotaLimits::default())
        .unwrap();

    assert!(matches!(
        meta.reserve_quota(request("two", "group-a", "sha256:shared", 8), QuotaLimits::default()),
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
    meta.reserve_quota(
        request("resource-a", "group-a", "sha256:first", 7),
        QuotaLimits::default(),
    )
    .unwrap();

    assert_eq!(meta.quota_usage("private").unwrap().accounted_bytes.reserved, 7);
}

#[test]
fn test_quota_release_by_allocation_frees_committed_counters() {
    let (_dir, meta) = store();
    let reservation = meta
        .reserve_quota(
            request("resource-a", "group-a", "sha256:first", 7),
            QuotaLimits::default(),
        )
        .unwrap();
    meta.commit_driver_txn_with_quota(reservation.id, |txn| {
        txn.put_local("blob/first", b"member")?;
        Ok::<_, QuotaError>(((), Vec::new()))
    })
    .unwrap();

    let removed = meta
        .commit_driver_txn_release_allocation(
            QuotaAllocation {
                repository: "private",
                resource: Some("resource-a"),
                group: Some("group-a"),
                digest: "sha256:first",
            },
            |removed| *removed,
            |txn| Ok::<_, QuotaError>((txn.remove("blob/first")?, Vec::new())),
        )
        .unwrap();

    assert_eq!(
        (
            removed,
            meta.get_driver_value("blob/first").unwrap(),
            meta.quota_usage("private").unwrap(),
            meta.quota_reservation(reservation.id).unwrap(),
        ),
        (true, None, QuotaUsage::default(), None)
    );
}

#[test]
fn test_quota_release_by_allocation_skips_when_the_row_is_absent() {
    let (_dir, meta) = store();
    let reservation = meta
        .reserve_quota(
            request("resource-a", "group-a", "sha256:first", 7),
            QuotaLimits::default(),
        )
        .unwrap();
    meta.commit_driver_txn_with_quota(reservation.id, |txn| {
        txn.put_local("blob/first", b"member")?;
        Ok::<_, QuotaError>(((), Vec::new()))
    })
    .unwrap();

    let removed = meta
        .commit_driver_txn_release_allocation(
            QuotaAllocation {
                repository: "private",
                resource: Some("resource-a"),
                group: Some("group-a"),
                digest: "sha256:first",
            },
            |removed| *removed,
            |txn| Ok::<_, QuotaError>((txn.remove("blob/absent")?, Vec::new())),
        )
        .unwrap();

    assert_eq!(
        (
            removed,
            meta.quota_reservation(reservation.id)
                .unwrap()
                .map(|record| record.state),
            meta.quota_usage("private").unwrap().accounted_bytes,
        ),
        (
            false,
            Some(QuotaReservationState::Committed),
            QuotaValue {
                committed: 7,
                reserved: 0,
            },
        )
    );
}

#[test]
fn test_quota_release_by_allocation_of_an_unmetered_row_changes_no_counters() {
    let (_dir, meta) = store();
    meta.commit_driver_txn(|txn| {
        txn.put_local("blob/first", b"member")?;
        Ok::<_, QuotaError>(((), Vec::new()))
    })
    .unwrap();

    let removed = meta
        .commit_driver_txn_release_allocation(
            QuotaAllocation {
                repository: "private",
                resource: Some("resource-a"),
                group: Some("group-a"),
                digest: "sha256:first",
            },
            |removed| *removed,
            |txn| Ok::<_, QuotaError>((txn.remove("blob/first")?, Vec::new())),
        )
        .unwrap();

    assert_eq!(
        (
            removed,
            meta.get_driver_value("blob/first").unwrap(),
            meta.quota_usage("private").unwrap(),
        ),
        (true, None, QuotaUsage::default())
    );
}

#[test]
fn test_quota_release_of_a_shared_digest_frees_bytes_only_after_the_last_reference() {
    let (_dir, meta) = store();
    let first = meta
        .reserve_quota(request("app", "group-a", "sha256:shared", 7), QuotaLimits::default())
        .unwrap();
    let second = meta
        .reserve_quota(request("api", "group-a", "sha256:shared", 7), QuotaLimits::default())
        .unwrap();
    for (id, key) in [(first.id, "blob/app"), (second.id, "blob/api")] {
        meta.commit_driver_txn_with_quota(id, |txn| {
            txn.put_local(key, b"member")?;
            Ok::<_, QuotaError>(((), Vec::new()))
        })
        .unwrap();
    }

    let release = |resource: &'static str, key: &'static str| {
        meta.commit_driver_txn_release_allocation(
            QuotaAllocation {
                repository: "private",
                resource: Some(resource),
                group: Some("group-a"),
                digest: "sha256:shared",
            },
            |removed| *removed,
            move |txn| Ok::<_, QuotaError>((txn.remove(key)?, Vec::new())),
        )
        .unwrap()
    };

    assert!(release("app", "blob/app"));
    assert_eq!(
        meta.quota_usage("private").unwrap().accounted_bytes,
        QuotaValue {
            committed: 7,
            reserved: 0,
        }
    );

    assert!(release("api", "blob/api"));
    assert_eq!(meta.quota_usage("private").unwrap(), QuotaUsage::default());
}

#[test]
fn test_quota_release_keeps_a_duplicate_allocations_own_index_entry() {
    let (_dir, meta) = store();
    let first = meta
        .reserve_quota(
            request("resource-a", "group-a", "sha256:shared", 7),
            QuotaLimits::default(),
        )
        .unwrap();
    let second = meta
        .reserve_quota(
            request("resource-a", "group-a", "sha256:shared", 7),
            QuotaLimits::default(),
        )
        .unwrap();
    meta.commit_quota_reservation(first.id).unwrap();
    meta.commit_quota_reservation(second.id).unwrap();

    meta.release_quota_reservation(first.id).unwrap();
    meta.commit_driver_txn_release_allocation(
        QuotaAllocation {
            repository: "private",
            resource: Some("resource-a"),
            group: Some("group-a"),
            digest: "sha256:shared",
        },
        |()| true,
        |_txn| Ok::<_, QuotaError>(((), Vec::new())),
    )
    .unwrap();

    assert_eq!(
        (
            meta.quota_reservation(second.id).unwrap(),
            meta.quota_usage("private").unwrap(),
        ),
        (None, QuotaUsage::default())
    );
}

fn request<'a>(resource: &'a str, group: &'a str, digest: &'a str, bytes: u64) -> NewQuotaReservation<'a> {
    NewQuotaReservation {
        repository: "private",
        resource: Some(resource),
        group: Some(group),
        digest,
        bytes,
        class: AccountingClass::Hosted,
        created_at_unix: 10,
    }
}
