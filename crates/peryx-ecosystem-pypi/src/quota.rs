use peryx_core::Role;
use peryx_driver::ServingState;
use peryx_events::metrics::{MetricFamily, Observation};
use peryx_index::Index;
use peryx_storage::meta::{AccountingClass, MetaStore, NewQuotaReservation, QuotaError, QuotaReservationRecord};

use crate::PackageName;

const QUOTA_ADMITTED_FAMILY: MetricFamily = MetricFamily {
    key: "quota_admitted",
    prom_name: "peryx_pypi_quota_admitted_total",
    help: "Hosted PyPI uploads admitted against a project quota.",
    ui_label: "Quota admitted uploads",
    roles: &[Role::Hosted],
    json_name: None,
    kind: peryx_events::metrics::MetricKind::Counter,
};

const QUOTA_REJECTED_FAMILY: MetricFamily = MetricFamily {
    key: "quota_rejected",
    prom_name: "peryx_pypi_quota_rejected_total",
    help: "Hosted PyPI uploads refused by a project quota.",
    ui_label: "Quota rejected uploads",
    roles: &[Role::Hosted],
    json_name: None,
    kind: peryx_events::metrics::MetricKind::Counter,
};

pub const QUOTA_FAMILIES: &[MetricFamily] = &[QUOTA_ADMITTED_FAMILY, QUOTA_REJECTED_FAMILY];

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
        resource: Some(project.as_str()),
        group: version,
        digest,
        bytes,
        class,
        created_at_unix,
    }
}

/// A pending upload allocation that releases itself when its request future is cancelled.
pub struct PendingQuota {
    meta: MetaStore,
    record: Option<QuotaReservationRecord>,
}

impl PendingQuota {
    #[must_use]
    pub(crate) const fn record(&self) -> &QuotaReservationRecord {
        self.record.as_ref().expect("a pending quota has not finished")
    }

    pub(crate) fn finish(&mut self) {
        self.record = None;
    }
}

impl Drop for PendingQuota {
    fn drop(&mut self) {
        let Some(record) = self.record.take() else {
            return;
        };
        let result = self.meta.release_quota_reservation(record.id);
        log_release_error(result.as_ref().err(), &record.id);
    }
}

fn log_release_error(err: Option<&QuotaError>, id: &dyn std::fmt::Display) {
    if let Some(err) = err {
        tracing::error!(%id, error = ?err, "failed to release cancelled upload quota");
    }
}

/// A metered upload's admission decision.
pub enum Admission {
    Reserved(PendingQuota),
    Rejected { total: u64 },
}

/// Reserve a project's upload bytes without scanning its stored files.
///
/// # Errors
/// Returns a quota-store or identity error. A configured limit rejection is returned as
/// [`Admission::Rejected`].
pub fn admit_upload(
    meta: &MetaStore,
    request: NewQuotaReservation<'_>,
    limit: u64,
    audit: bool,
) -> Result<Admission, QuotaError> {
    match meta.reserve_resource_quota(request, limit, audit) {
        Ok(record) => Ok(Admission::Reserved(PendingQuota {
            meta: meta.clone(),
            record: Some(record),
        })),
        Err(QuotaError::ResourceExceeded { total }) => Ok(Admission::Rejected { total }),
        Err(err) => Err(err),
    }
}

pub fn record_decision(state: &ServingState, index: &Index, project: &str, rejected: bool) {
    state.metrics.record(Observation::Ecosystem {
        repository: index.route.clone(),
        resource: project.to_owned(),
        artifact: None,
        family: if rejected {
            QUOTA_REJECTED_FAMILY.key
        } else {
            QUOTA_ADMITTED_FAMILY.key
        },
    });
}

#[cfg(test)]
#[path = "../tests/unit/quota/tests.rs"]
mod tests;
