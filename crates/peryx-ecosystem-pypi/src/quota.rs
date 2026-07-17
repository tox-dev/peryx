use peryx_storage::meta::{AccountingClass, NewQuotaReservation};

use crate::PackageName;

/// Use the PEP 503 project key for quota accounting across equivalent name spellings.
#[must_use]
pub const fn quota_reservation<'a>(
    repository: &'a str,
    project: &'a PackageName,
    version: Option<&'a str>,
    digest: &'a str,
    bytes: u64,
    class: AccountingClass,
    created_at_unix: i64,
) -> NewQuotaReservation<'a> {
    NewQuotaReservation {
        repository,
        project: Some(project.as_str()),
        version,
        digest,
        bytes,
        class,
        created_at_unix,
    }
}
