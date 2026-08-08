use peryx_storage::meta::{AccountingClass, MetaStore, NewQuotaReservation, QuotaReservationState};

use crate::quota::{Admission, admit_upload};
use crate::{PackageName, quota_reservation};

#[test]
fn test_quota_reservation_normalizes_project_identity() {
    let project = PackageName::new("Zope.Interface");

    assert_eq!(
        quota_reservation(
            "private",
            &project,
            Some("7.2"),
            "sha256:abc",
            42,
            AccountingClass::Hosted,
            100,
        ),
        NewQuotaReservation {
            repository: "private",
            project: Some("zope-interface"),
            version: Some("7.2"),
            digest: "sha256:abc",
            bytes: 42,
            class: AccountingClass::Hosted,
            created_at_unix: 100,
        }
    );
}

#[test]
fn test_quota_admission_commits_project_bytes() {
    let (_dir, meta) = store();
    let project = PackageName::new("Flask");
    let Admission::Reserved(mut pending) =
        admit_upload(&meta, request(&project, "1.0", "sha256:first", 7, 1), 8, false).unwrap()
    else {
        panic!("an upload within the limit must reserve capacity");
    };
    let id = pending.record().id;

    meta.commit_quota_reservation(id).unwrap();
    pending.finish();

    assert_eq!(
        (
            meta.quota_project_usage("private", "flask")
                .unwrap()
                .file_bytes
                .committed,
            meta.quota_reservation(id).unwrap().unwrap().state,
        ),
        (7, QuotaReservationState::Committed)
    );
}

#[test]
fn test_quota_admission_rejects_the_projected_total() {
    let (_dir, meta) = store();
    let project = PackageName::new("flask");
    let first = quota_reservation(
        "private",
        &project,
        Some("1.0"),
        "sha256:first",
        7,
        AccountingClass::Hosted,
        1,
    );
    let first = meta.reserve_project_quota(first, 10, false).unwrap();
    meta.commit_quota_reservation(first.id).unwrap();

    let outcome = admit_upload(&meta, request(&project, "2.0", "sha256:second", 4, 2), 10, false).unwrap();

    assert!(matches!(outcome, Admission::Rejected { total: 11 }));
    assert_eq!(
        meta.quota_project_usage("private", "flask")
            .unwrap()
            .file_bytes
            .reserved,
        0
    );
}

#[test]
fn test_quota_audit_admits_and_records_a_violation() {
    let (_dir, meta) = store();
    let project = PackageName::new("flask");
    let Admission::Reserved(mut pending) =
        admit_upload(&meta, request(&project, "1.0", "sha256:first", 7, 1), 6, true).unwrap()
    else {
        panic!("audit mode must admit a would-reject upload");
    };
    let id = pending.record().id;

    meta.commit_quota_reservation(id).unwrap();
    pending.finish();

    assert_eq!(meta.quota_reservation(id).unwrap().unwrap().violations.len(), 1);
}

#[test]
fn test_quota_pending_drop_releases_cancelled_capacity() {
    let (_dir, meta) = store();
    let project = PackageName::new("flask");
    let Admission::Reserved(pending) =
        admit_upload(&meta, request(&project, "1.0", "sha256:first", 7, 1), 8, false).unwrap()
    else {
        panic!("an upload within the limit must reserve capacity");
    };

    drop(pending);

    assert_eq!(
        meta.quota_project_usage("private", "flask")
            .unwrap()
            .file_bytes
            .reserved,
        0
    );
}

#[test]
fn test_quota_admission_returns_identity_errors() {
    let (_dir, meta) = store();
    let project = PackageName::new(&"a".repeat(513));

    let Err(error) = admit_upload(&meta, request(&project, "1.0", "sha256:first", 7, 1), 8, false) else {
        panic!("an oversized identity must fail admission");
    };

    assert_eq!(error.to_string(), "project exceeds 512 bytes");
}

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, meta)
}

const fn request<'a>(
    project: &'a PackageName,
    version: &'a str,
    digest: &'a str,
    bytes: u64,
    created_at_unix: i64,
) -> NewQuotaReservation<'a> {
    quota_reservation(
        "private",
        project,
        Some(version),
        digest,
        bytes,
        AccountingClass::Hosted,
        created_at_unix,
    )
}
