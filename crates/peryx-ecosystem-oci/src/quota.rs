//! Repository-quota enforcement for the hosted push paths.
//!
//! A blob upload, cross-repo mount, or manifest publication reserves capacity against the substrate
//! before it becomes discoverable, commits that reservation atomically with the metadata write, and
//! releases it when the write fails. An index that configures no quota keeps its original write path,
//! so an unmetered registry pays nothing for the machinery.

use axum::response::Response;
use peryx_core::Role;
use peryx_driver::ServingState;
use peryx_events::metrics::{Event, MetricFamily};
use peryx_index::Index;
use peryx_policy::Policy;
use peryx_storage::meta::{
    AccountingClass, DriverTxn, MetaStore, NewQuotaReservation, QuotaError, QuotaLimit, QuotaLimits,
    QuotaReservationRecord,
};

use crate::error::{ErrorCode, error_response};
use crate::name::Reference;
use crate::registry::ServeError;
use crate::store::{self, Manifest};

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

/// A hosted push admitted against the repository quota.
const QUOTA_ADMITTED_FAMILY: MetricFamily = MetricFamily {
    key: "quota_admitted",
    prom_name: "peryx_oci_quota_admitted_total",
    help: "Hosted OCI pushes admitted against the repository quota.",
    ui_label: "Quota admitted pushes",
    roles: &[Role::Hosted],
};

/// A hosted push refused by the repository quota.
const QUOTA_REJECTED_FAMILY: MetricFamily = MetricFamily {
    key: "quota_rejected",
    prom_name: "peryx_oci_quota_rejected_total",
    help: "Hosted OCI pushes refused by the repository quota.",
    ui_label: "Quota rejected pushes",
    roles: &[Role::Hosted],
};

/// The quota-decision counters the OCI driver publishes.
pub const QUOTA_FAMILIES: &[MetricFamily] = &[QUOTA_ADMITTED_FAMILY, QUOTA_REJECTED_FAMILY];

/// The outcome of admitting a hosted push against the repository quota.
pub enum Admission {
    /// The index configures no quota; publish without accounting.
    Unmetered,
    /// The push is admitted. Commit the reservation with the publication, or release it on failure.
    Reserved(QuotaReservationRecord),
    /// The push is refused. Return this distribution-spec error to the client.
    Rejected(Response),
}

/// Reserve repository capacity for a hosted push and record the decision metric.
///
/// Returns [`Admission::Unmetered`] when the index sets no quota, so an unconfigured registry keeps
/// its original write path. In audit mode the reservation is admitted even when it crosses a limit,
/// and its recorded violations stay on the durable reservation record for inspection.
pub fn admit_push(
    state: &ServingState,
    index: &Index,
    repo: &str,
    version: Option<&str>,
    digest: &str,
    bytes: u64,
) -> Result<Admission, ServeError> {
    let Some(limits) = quota_limits(&index.policy) else {
        return Ok(Admission::Unmetered);
    };
    let request = quota_reservation(
        &index.name,
        repo,
        version,
        digest,
        bytes,
        AccountingClass::Hosted,
        (state.clock)(),
    );
    match reserve(&state.meta, request, limits)? {
        ReserveOutcome::Admitted(record) => {
            record_quota_metric(state, index, repo, QUOTA_ADMITTED_FAMILY.key);
            Ok(Admission::Reserved(record))
        }
        ReserveOutcome::Rejected(violations) => {
            record_quota_metric(state, index, repo, QUOTA_REJECTED_FAMILY.key);
            Ok(Admission::Rejected(error_response(
                ErrorCode::Denied,
                &format!("repository quota exceeded: {}", describe(&violations)),
            )))
        }
    }
}

/// The storage limit set an index configures, or `None` when it accounts for nothing. The per-file
/// size limit is enforced on the byte stream itself, so it alone does not switch accounting on.
fn quota_limits(policy: &Policy) -> Option<QuotaLimits> {
    policy.enforces_quota().then(|| QuotaLimits {
        max_file_bytes: policy.max_file_size(),
        max_accounted_bytes: policy.max_accounted_bytes(),
        max_projects: policy.max_projects(),
        max_versions_per_project: policy.max_versions_per_project(),
        audit: policy.quota_audit(),
    })
}

/// The reservation decision, separated from request state so the enforce, audit, and fault branches
/// are exercised against a bare [`MetaStore`].
enum ReserveOutcome {
    Admitted(QuotaReservationRecord),
    Rejected(Vec<QuotaLimit>),
}

fn reserve(
    meta: &MetaStore,
    request: NewQuotaReservation<'_>,
    limits: QuotaLimits,
) -> Result<ReserveOutcome, ServeError> {
    match meta.reserve_quota(request, limits) {
        Ok(record) => Ok(ReserveOutcome::Admitted(record)),
        Err(QuotaError::Exceeded { violations }) => Ok(ReserveOutcome::Rejected(violations)),
        Err(err) => Err(err.into()),
    }
}

fn record_quota_metric(state: &ServingState, index: &Index, repo: &str, family: &'static str) {
    state.metrics.record(Event::Ecosystem {
        route: index.route.clone(),
        project: repo.to_owned(),
        filename: None,
        family,
    });
}

fn describe(violations: &[QuotaLimit]) -> String {
    violations
        .iter()
        .map(|limit| match limit {
            QuotaLimit::FileBytes => "file size",
            QuotaLimit::AccountedBytes => "repository bytes",
            QuotaLimit::Projects => "repository projects",
            QuotaLimit::VersionsPerProject => "project versions",
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Publish a blob's `(index, repo)` membership, committing a quota reservation with it when the push
/// was metered so the two land in one transaction.
pub fn commit_blob_membership(
    meta: &MetaStore,
    index: &str,
    repo: &str,
    digest: &str,
    reservation: Option<QuotaReservationRecord>,
) -> Result<(), ServeError> {
    match reservation {
        None => Ok(store::record_blob_membership(meta, index, repo, digest)?),
        Some(record) => meta.commit_driver_txn_with_quota(record.id, |txn| {
            txn.put(&store::blob_membership_key(index, repo, digest), &[])?;
            Ok::<_, ServeError>(((), Vec::new()))
        }),
    }
}

/// Publish a manifest by digest and optional tag, committing a quota reservation with it when the
/// push was metered. Reports whether the searchable tag set grew.
pub fn publish_manifest(
    meta: &MetaStore,
    index: &str,
    repo: &str,
    canonical: &str,
    manifest: &Manifest,
    reference: &Reference,
    reservation: Option<QuotaReservationRecord>,
) -> Result<bool, ServeError> {
    let body = |txn: &mut DriverTxn| -> Result<(bool, Vec<Vec<u8>>), ServeError> {
        store::record_manifest_txn(txn, index, repo, canonical, manifest)?;
        let grew = match reference {
            Reference::Tag(tag) => store::put_tag_txn(txn, index, repo, tag, canonical)?,
            Reference::Digest(_) => false,
        };
        Ok((grew, Vec::new()))
    };
    match reservation {
        None => meta.commit_driver_txn(body),
        Some(record) => meta.commit_driver_txn_with_quota(record.id, body),
    }
}

/// Whether this exact manifest is already published under `reference`, so a re-push is a no-op that
/// must not account a fresh version or byte allocation.
pub fn manifest_already_published(
    meta: &MetaStore,
    index: &str,
    repo: &str,
    canonical: &str,
    reference: &Reference,
) -> Result<bool, ServeError> {
    if !store::manifest_is_member(meta, index, repo, canonical)? {
        return Ok(false);
    }
    match reference {
        Reference::Digest(_) => Ok(true),
        Reference::Tag(tag) => Ok(store::get_tag(meta, index, repo, tag)?.as_deref() == Some(canonical)),
    }
}

#[cfg(test)]
mod tests {
    use peryx_storage::meta::MetaStore;

    use super::{ReserveOutcome, describe, quota_reservation, reserve};
    use peryx_storage::meta::{AccountingClass, QuotaLimit, QuotaLimits};

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
        // A repository key past the substrate's identity length ceiling is a hard fault, not a quota
        // decision, so it propagates rather than reads as an admission or a rejection.
        let long = "r".repeat(600);
        let request = quota_reservation(&long, "app", None, "sha256:a", 1, AccountingClass::Hosted, 1);
        assert!(reserve(&meta, request, QuotaLimits::default()).is_err());
    }

    #[test]
    fn test_describe_names_each_crossed_counter() {
        assert_eq!(
            describe(&[
                QuotaLimit::FileBytes,
                QuotaLimit::AccountedBytes,
                QuotaLimit::Projects,
                QuotaLimit::VersionsPerProject,
            ]),
            "file size, repository bytes, repository projects, project versions"
        );
    }
}
