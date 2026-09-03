use peryx_core::Role;
use peryx_driver::ServingState;
use peryx_events::metrics::{MetricFamily, Observation};
use peryx_index::Index;
use peryx_policy::Policy;
use peryx_storage::meta::{
    AccountingClass, MetaStore, NewQuotaReservation, QuotaError, QuotaLimit, QuotaLimits, QuotaReservationRecord,
};

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
    created_at_unix: i64,
) -> NewQuotaReservation<'a> {
    NewQuotaReservation {
        repository,
        resource: Some(project.as_str()),
        group: version,
        digest,
        bytes,
        class: AccountingClass::Hosted,
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
    Rejected(QuotaRejection),
}

pub enum QuotaRejection {
    ProjectBytes { total: u64 },
    Limits(Vec<QuotaLimit>),
}

/// Reserve a project's upload bytes without scanning its stored files.
///
/// # Errors
/// Returns a quota-store or identity error. A configured limit rejection is returned as
/// [`Admission::Rejected`].
pub fn admit_upload(
    meta: &MetaStore,
    request: NewQuotaReservation<'_>,
    limits: QuotaLimits,
    max_project_bytes: Option<u64>,
) -> Result<Admission, QuotaError> {
    match meta.reserve_resource_quota(request, limits, max_project_bytes) {
        Ok(record) => Ok(Admission::Reserved(PendingQuota {
            meta: meta.clone(),
            record: Some(record),
        })),
        Err(QuotaError::ResourceExceeded { total }) => Ok(Admission::Rejected(QuotaRejection::ProjectBytes { total })),
        Err(QuotaError::Exceeded { violations }) => Ok(Admission::Rejected(QuotaRejection::Limits(violations))),
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

/// The quota a write faces on one route: the named index's limits merged with the hosted layer it
/// writes through, since a virtual route answers to both.
#[derive(Clone, Copy)]
pub struct EffectiveQuota {
    pub limits: QuotaLimits,
    pub max_project_bytes: Option<u64>,
}
#[must_use]
pub fn effective_project_quota(index: &Index, hosted: &Index) -> Option<EffectiveQuota> {
    match (
        policy_quota(&index.policy),
        (hosted.name != index.name)
            .then(|| policy_quota(&hosted.policy))
            .flatten(),
    ) {
        (Some(index), Some(hosted)) => Some(merge_quotas(index, hosted)),
        (Some(quota), None) | (None, Some(quota)) => Some(quota),
        (None, None) => None,
    }
}
fn policy_quota(policy: &Policy) -> Option<EffectiveQuota> {
    (policy.enforces_quota() || policy.has_resource_size_limit()).then(|| EffectiveQuota {
        limits: QuotaLimits {
            max_artifact_bytes: policy.max_artifact_size(),
            max_accounted_bytes: policy.max_accounted_bytes(),
            max_resources: policy.max_resources(),
            max_groups_per_resource: policy.max_groups_per_resource(),
            audit: policy.quota_audit(),
        },
        max_project_bytes: policy.max_resource_size(),
    })
}
fn merge_quotas(first: EffectiveQuota, second: EffectiveQuota) -> EffectiveQuota {
    EffectiveQuota {
        limits: QuotaLimits {
            max_artifact_bytes: minimum(first.limits.max_artifact_bytes, second.limits.max_artifact_bytes),
            max_accounted_bytes: minimum(first.limits.max_accounted_bytes, second.limits.max_accounted_bytes),
            max_resources: minimum(first.limits.max_resources, second.limits.max_resources),
            max_groups_per_resource: minimum(
                first.limits.max_groups_per_resource,
                second.limits.max_groups_per_resource,
            ),
            audit: first.limits.audit && second.limits.audit,
        },
        max_project_bytes: minimum(first.max_project_bytes, second.max_project_bytes),
    }
}
fn minimum(first: Option<u64>, second: Option<u64>) -> Option<u64> {
    first.into_iter().chain(second).min()
}

#[cfg(test)]
#[path = "../tests/unit/quota/tests.rs"]
mod tests;
