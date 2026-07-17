use peryx_storage::meta::{AccountingClass, NewQuotaReservation};

/// Account the OCI repository path as a project and an optional tag as its version.
#[must_use]
pub const fn quota_reservation<'a>(
    repository: &'a str,
    name: &'a str,
    tag: Option<&'a str>,
    digest: &'a str,
    bytes: u64,
    class: AccountingClass,
    created_at_unix: i64,
) -> NewQuotaReservation<'a> {
    NewQuotaReservation {
        repository,
        project: Some(name),
        version: tag,
        digest,
        bytes,
        class,
        created_at_unix,
    }
}
