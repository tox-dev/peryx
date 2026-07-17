use peryx_storage::meta::{AccountingClass, NewQuotaReservation};

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
