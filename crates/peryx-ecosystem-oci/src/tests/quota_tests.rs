use peryx_storage::meta::{AccountingClass, NewQuotaReservation};

use crate::quota_reservation;

#[test]
fn test_quota_reservation_preserves_oci_identity() {
    for (case, tag) in [("tagged manifest", Some("stable")), ("blob", None)] {
        assert_eq!(
            (
                case,
                quota_reservation(
                    "images",
                    "team/api",
                    tag,
                    "sha256:abc",
                    42,
                    AccountingClass::Generated,
                    100,
                ),
            ),
            (
                case,
                NewQuotaReservation {
                    repository: "images",
                    project: Some("team/api"),
                    version: tag,
                    digest: "sha256:abc",
                    bytes: 42,
                    class: AccountingClass::Generated,
                    created_at_unix: 100,
                },
            )
        );
    }
}
