//! Finalizes admitted `PyPI` uploads at their authority home.
//!
//! Finalization is fenced and idempotent. A stale authority epoch, a permission the intent no longer
//! holds, a placement that does not record the bytes as arrived, or a checksum that disagrees with what
//! was admitted each rejects the finalize before publication. The placement check demands proof rather
//! than reading an absent row as proof of the opposite: a missing row is no observation at all, so
//! finalize refuses and the upload retries once the row it needs exists. Only the publish is durable: it runs through
//! [`commit_finalized_write`](peryx_storage::meta::MetaStore::commit_finalized_write), which stamps the
//! operation's terminal outcome in the same transaction as the metadata, so a retry after a timeout or a
//! restart replays the one committed result rather than publishing twice. A refusal carries no such
//! record, so an upload blocked on a condition that later clears finalizes on its next attempt.
//!
//! Routing an admitted intent to its home and moving the bytes there is [#541]'s job and excluded here;
//! this module is the mechanism that home invokes, and a test drives it directly with a descriptor
//! standing in for what routing supplies.
//!
//! [#541]: https://github.com/tox-dev/peryx/issues/541

use peryx_driver::state::ServingState;
use peryx_identity::{Action, Principal, authorize};
use peryx_storage::meta::{FinalizedWrite, MetaError};

use crate::store::{Guard, PublishedFile, publish_file_in_txn};

/// The body a durable finalize stores and replays, matching the acknowledgement a synchronous upload
/// returns once its write is proven durable.
const DURABLE_ACK: &[u8] = b"upload accepted";

/// Everything the home DC needs to publish an admitted upload, standing in for what [#541] routing
/// supplies. A test constructs it directly; the transport of the bytes it names is out of scope here.
///
/// [#541]: https://github.com/tox-dev/peryx/issues/541
pub struct FinalizeDescriptor<'a> {
    /// The idempotency key the admission minted for this upload, whose terminal outcome a retry replays.
    pub operation: &'a str,
    /// The authority whose home fences this finalize by committed epoch.
    pub authority: &'a str,
    /// The principal the upload authenticated as, re-authorized against the current ACL in case a
    /// permission changed between admission and finalization.
    pub principal: &'a Principal,
    /// The hosted index that owns the write.
    pub index_name: &'a str,
    /// The project's normalized name, which keys its rows.
    pub normalized: &'a str,
    /// The project's display name, as the uploader spelled it.
    pub display: &'a str,
    /// The distribution filename.
    pub filename: &'a str,
    /// The artifact's sha256, checked against the admitted intent's digest.
    pub artifact_sha256: &'a str,
    /// The artifact's byte length, checked against the admitted intent's size.
    pub artifact_size: u64,
    /// The serialized file record served on the project's page.
    pub record: &'a [u8],
    /// The release the file belongs to.
    pub version: &'a str,
    /// When the upload was submitted, as Unix seconds.
    pub submitted_at_unix: i64,
    /// When the stored operation outcome may be pruned, or `None` for no bound.
    pub expiry_unix: Option<i64>,
}

/// A finalize's committed verdict. Only a published release is durable; a validation refusal is a
/// transient [`FinalizeError`], re-evaluated on retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finalization {
    /// The upload committed for the first time; `response` is the acknowledgement a retry replays.
    Published { response: Vec<u8> },
    /// A prior finalize already committed this operation; its stored acknowledgement is replayed with no
    /// second write.
    Replayed { response: Vec<u8> },
}

/// Why a finalize refused to publish. A refusal is not durable: a retry re-evaluates it, so an upload
/// blocked on a condition that later clears finalizes on its next attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalizeFailure {
    /// The authority's committed epoch does not admit this finalize: its home moved or was never
    /// assigned.
    Fenced,
    /// The principal no longer holds write on the index.
    Unauthorized,
    /// The artifact's bytes are not placed, so publishing would serve a file no store holds.
    MissingPlacement,
    /// The descriptor's digest or size disagrees with what was admitted.
    ChecksumMismatch,
}

/// Why a finalize did not publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinalizeError {
    /// No intent is staged under the key, so there is nothing to finalize.
    NotStaged,
    /// A validation refused publication. A retry re-evaluates the same check.
    Rejected(FinalizeFailure),
    /// The store failed to read, validate, or commit.
    Store(String),
}

impl From<MetaError> for FinalizeError {
    fn from(err: MetaError) -> Self {
        Self::Store(err.to_string())
    }
}

/// # Errors
/// Returns [`FinalizeError::NotStaged`] when no intent is staged, [`FinalizeError::Rejected`] when a
/// validation refuses publication, and [`FinalizeError::Store`] when the store fails. A refusal is
/// transient: a retry re-evaluates it.
pub async fn finalize_admitted_upload(
    state: &ServingState,
    intent_key: &str,
    descriptor: &FinalizeDescriptor<'_>,
) -> Result<Finalization, FinalizeError> {
    let Some(intent) = state.meta.staged_intent(intent_key)? else {
        return Err(FinalizeError::NotStaged);
    };

    if let Some(record) = state
        .meta
        .operation_outcome(descriptor.operation)?
        .filter(|record| record.state.is_terminal())
    {
        return Ok(Finalization::Replayed {
            response: record.response,
        });
    }

    let epoch = state.committed_authority_epoch(descriptor.authority).await;
    if descriptor.artifact_sha256 != intent.digest || descriptor.artifact_size != intent.size {
        return Err(FinalizeError::Rejected(FinalizeFailure::ChecksumMismatch));
    }
    // Publishing needs positive evidence that the bytes reached this home. An absent row is not that
    // evidence, and it is not evidence of the reverse either, so this fails closed and the retry
    // succeeds once the blob plane records the arrival.
    if state.meta.get_artifact_placement(&intent.digest)?.is_none() {
        return Err(FinalizeError::Rejected(FinalizeFailure::MissingPlacement));
    }
    if !authorized(state, descriptor) {
        return Err(FinalizeError::Rejected(FinalizeFailure::Unauthorized));
    }

    let lease = match state.begin_authority_epoch_write(descriptor.authority, epoch).await {
        Ok(lease) if lease.is_some() || state.ownership_authority().is_none() => lease,
        Ok(_) | Err(_) => return Err(FinalizeError::Rejected(FinalizeFailure::Fenced)),
    };
    let result = publish(state, intent_key, descriptor, lease.as_ref(), (state.clock)());
    if let Some(lease) = lease
        && let Err(error) = state.finish_authority_epoch_write(&lease).await
    {
        tracing::warn!(%error, authority = lease.authority, "authority write lease release failed");
    }
    result
}

/// Whether the descriptor's principal holds write on its index under the index's current ACL. An
/// unknown index fails closed, since no ACL grants a write there.
fn authorized(state: &ServingState, descriptor: &FinalizeDescriptor<'_>) -> bool {
    state
        .indexes
        .iter()
        .find(|index| index.name == descriptor.index_name)
        .is_some_and(|index| {
            authorize(
                descriptor.principal,
                &index.acl,
                peryx_identity::ResourceMatch::Pattern(descriptor.normalized),
                Action::Write,
            )
            .is_ok()
        })
}

/// Publish the file's release rows, its outbox journal, the operation's terminal outcome, and the
/// intent's advance in one transaction through the finalize primitive. The upload's bytes were already
/// stored where it was admitted, so a re-published filename is an idempotent no-op that still commits
/// the terminal outcome and advances the intent.
fn publish(
    state: &ServingState,
    intent_key: &str,
    descriptor: &FinalizeDescriptor<'_>,
    lease: Option<&peryx_ha::AuthorityWriteLease>,
    now: i64,
) -> Result<Finalization, FinalizeError> {
    let file = PublishedFile {
        index: descriptor.index_name,
        normalized: descriptor.normalized,
        display: descriptor.display,
        filename: descriptor.filename,
        artifact_sha256: descriptor.artifact_sha256,
        artifact_size: descriptor.artifact_size,
        record: descriptor.record,
        version: descriptor.version,
        submitted_at_unix: descriptor.submitted_at_unix,
        metadata: None,
        provenance: None,
        quota: None,
    };
    let write = FinalizedWrite {
        operation: descriptor.operation,
        intent_key,
        response: DURABLE_ACK,
        expiry_unix: descriptor.expiry_unix,
        now,
    };
    let guard = |stored: crate::store::PublishedState<'_>| {
        Ok::<_, FinalizeError>(if stored.record.is_some() {
            Guard::Skip
        } else {
            Guard::Commit
        })
    };
    // The terminal-outcome replay is decided before publication, so this commit publishes rather than
    // replays; either way the operation ends published with the same acknowledgement.
    state.meta.commit_finalized_write(write, |txn| {
        if lease.is_some_and(|lease| !lease.admits((state.clock)())) {
            return Err(FinalizeError::Rejected(FinalizeFailure::Fenced));
        }
        publish_file_in_txn::<FinalizeError>(txn, crate::replication_enabled(state), &file, guard, None)
            .map(|(_, journal)| journal)
    })?;
    Ok(Finalization::Published {
        response: DURABLE_ACK.to_vec(),
    })
}

#[cfg(test)]
#[path = "../../tests/unit/serving/finalize/tests.rs"]
mod tests;
