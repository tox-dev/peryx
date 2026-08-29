//! A blob upload, cross-repo mount, or manifest publication reserves capacity against the substrate
//! before it becomes discoverable, commits that reservation atomically with the metadata write, and
//! releases it when the write fails. An index that configures no quota keeps its original write path,
//! so an unmetered registry pays nothing for the machinery.

use axum::response::Response;
use peryx_core::Role;
use peryx_driver::ServingState;
use peryx_driver::quota::quota_limit_label;
use peryx_events::metrics::{MetricFamily, Observation};
use peryx_index::Index;
use peryx_policy::Policy;
use peryx_storage::meta::{
    AccountingClass, DriverTxn, MetaStore, NewQuotaReservation, QuotaAllocation, QuotaError, QuotaLimit, QuotaLimits,
    QuotaReservationRecord,
};

use crate::upload_session::UploadStore as _;

use crate::OCI_LEXICON;
use crate::error::{ErrorCode, error_response};
use crate::name::Reference;
use crate::registry::ServeError;
use crate::store::{self, Manifest};

#[must_use]
pub const fn quota_reservation<'a>(
    repository: &'a str,
    name: &'a str,
    tag: Option<&'a str>,
    digest: &'a str,
    bytes: u64,
    created_at_unix: i64,
) -> NewQuotaReservation<'a> {
    NewQuotaReservation {
        repository,
        resource: Some(name),
        group: tag,
        digest,
        bytes,
        class: AccountingClass::Hosted,
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
    json_name: None,
    kind: peryx_events::metrics::MetricKind::Counter,
};

/// A hosted push refused by the repository quota.
const QUOTA_REJECTED_FAMILY: MetricFamily = MetricFamily {
    key: "quota_rejected",
    prom_name: "peryx_oci_quota_rejected_total",
    help: "Hosted OCI pushes refused by the repository quota.",
    ui_label: "Quota rejected pushes",
    roles: &[Role::Hosted],
    json_name: None,
    kind: peryx_events::metrics::MetricKind::Counter,
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
    let request = quota_reservation(&index.name, repo, version, digest, bytes, (state.clock)());
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
        max_artifact_bytes: policy.max_artifact_size(),
        max_accounted_bytes: policy.max_accounted_bytes(),
        max_resources: policy.max_resources(),
        max_groups_per_resource: policy.max_groups_per_resource(),
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
    state.metrics.record(Observation::Ecosystem {
        repository: index.route.clone(),
        resource: repo.to_owned(),
        artifact: None,
        family,
    });
}

fn describe(violations: &[QuotaLimit]) -> String {
    violations
        .iter()
        .map(|limit| quota_limit_label(&OCI_LEXICON, *limit))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Publish a blob's `(index, repo)` membership, committing a quota reservation with it when the push
/// was metered so the two land in one transaction. A finalizing resumable upload names its `session`,
/// closed in that same transaction so membership never lands while the client's recovery handle
/// lingers; a mount or a monolithic push passes `None`.
pub fn commit_blob_membership(
    meta: &MetaStore,
    index: &str,
    repo: &str,
    digest: &str,
    reservation: Option<QuotaReservationRecord>,
    session: Option<&str>,
    journal: crate::outbox::Outbox,
) -> Result<(), ServeError> {
    finalize(meta, reservation, session, |txn| {
        txn.put(&store::blob_membership_key(index, repo, digest), &[])?;
        let entries = crate::outbox::record(journal, || crate::outbox::OciMutation::MountBlob {
            index: index.to_owned(),
            repo: repo.to_owned(),
            digest: digest.to_owned(),
        });
        Ok(((), entries))
    })
}

/// Delete a blob's `(index, repo)` membership and release the committed quota allocation its push
/// charged, in one metadata transaction so a crash cannot leave the repository billed for content it
/// no longer serves. An unmetered blob has no allocation to release. Reports whether the membership
/// existed.
pub fn release_blob_membership(meta: &MetaStore, index: &str, repo: &str, digest: &str) -> Result<bool, ServeError> {
    let allocation = QuotaAllocation {
        repository: index,
        resource: Some(repo),
        group: None,
        digest,
    };
    meta.commit_driver_txn_release_allocation(
        allocation,
        |deleted| *deleted,
        |txn| {
            Ok((
                txn.remove(&store::blob_membership_key(index, repo, digest))?,
                Vec::new(),
            ))
        },
    )
}

pub struct ManifestCommit<'a> {
    pub index: &'a str,
    pub repo: &'a str,
    pub canonical: &'a str,
    pub manifest: &'a Manifest,
    pub reference: &'a Reference,
    pub reservation: Option<QuotaReservationRecord>,
    pub journal: crate::outbox::Outbox,
}

pub fn publish_manifest(meta: &MetaStore, commit: ManifestCommit<'_>) -> Result<bool, ServeError> {
    let ManifestCommit {
        index,
        repo,
        canonical,
        manifest,
        reference,
        reservation,
        journal,
    } = commit;
    let body = |txn: &mut DriverTxn| -> Result<(bool, Vec<Vec<u8>>), ServeError> {
        let tag = match reference {
            Reference::Tag(tag) => Some(tag.as_str()),
            Reference::Digest(_) => None,
        };
        let changed = store::publish_manifest_txn(txn, index, repo, canonical, manifest, tag)?;
        let entries = crate::outbox::record(journal, || crate::outbox::OciMutation::PublishManifest {
            index: index.to_owned(),
            repo: repo.to_owned(),
            digest: canonical.to_owned(),
            tag: tag.map(str::to_owned),
        });
        Ok((changed, entries))
    };
    finalize(meta, reservation, None, body)
}

fn finalize<T>(
    meta: &MetaStore,
    reservation: Option<QuotaReservationRecord>,
    session: Option<&str>,
    body: impl FnOnce(&mut DriverTxn) -> Result<(T, Vec<Vec<u8>>), ServeError>,
) -> Result<T, ServeError> {
    let Some(record) = reservation else {
        return meta.commit_driver_txn_closing_upload(session, body);
    };
    match meta.commit_driver_txn_with_quota_closing_upload(record.id, session, body) {
        Ok(value) => Ok(value),
        Err(err) => {
            meta.release_quota_reservation(record.id)?;
            Err(err)
        }
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
        Reference::Tag(tag) => Ok(store::get_tag(meta, index, repo, tag)?.as_deref() == Some(canonical)
            || store::trashed_tag_digest(meta, index, repo, tag)?.as_deref() == Some(canonical)),
    }
}

#[cfg(test)]
#[path = "../tests/unit/quota/tests.rs"]
mod tests;
