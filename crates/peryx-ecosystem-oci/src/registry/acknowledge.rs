//! OCI writes wait for `[availability.write_ack]` before answering success. Blob acknowledgements carry
//! byte and metadata evidence; manifest acknowledgements carry a journal frontier because metadata holds
//! their bytes. An unresolved verdict leaves the operation pending for a retry.

use axum::http::header;
use axum::response::Response;
use peryx_driver::ServingState;
use peryx_ha::{AuthorityEpoch, CommittedBlob, CommittedMetadata, WriteDurability};
use peryx_storage::blob::{Digest, WriteEvidence};
use peryx_storage::meta::JournalCommit;

use crate::error::{ErrorCode, error_response};

/// A committed OCI blob write awaiting its durability decision.
pub(in crate::registry) struct BlobAck<'a> {
    pub(in crate::registry) repo: &'a str,
    pub(in crate::registry) digest: &'a Digest,
    pub(in crate::registry) bytes: u64,
    /// The journal serial the membership row committed at, which the metadata dimension waits on.
    /// `None` when the mutation journaled nothing, leaving only the byte dimension to prove.
    pub(in crate::registry) commit: Option<JournalCommit>,
    pub(in crate::registry) evidence: WriteEvidence,
}

pub(in crate::registry) struct MetadataAck<'a> {
    pub(in crate::registry) repo: &'a str,
    pub(in crate::registry) epoch: u64,
    pub(in crate::registry) commit: JournalCommit,
}

/// Resolve the configured acknowledgement policy for a committed write.
///
/// `Ok` means the policy passed and the caller may publish the operation and answer the endpoint's
/// success code. `Err` carries the retry response for a write whose durability is still unproven; the
/// caller must leave the operation pending so the retry finishes it.
pub(in crate::registry) async fn acknowledge_blob(state: &ServingState, ack: BlobAck<'_>) -> Result<(), Response> {
    let authority = crate::name::authority_key(ack.repo);
    let digest = ack.digest.as_str();
    let epoch = AuthorityEpoch(state.committed_authority_epoch(&authority).await);
    let durability = state
        .confirm_blob_write(CommittedBlob::new(
            ack.digest,
            ack.bytes,
            &authority,
            epoch,
            ack.commit,
            ack.evidence,
        ))
        .await;
    match durability {
        WriteDurability::Confirmed { scope } => {
            // Bound outside the macro: a field expression runs only while a subscriber is enabled.
            let scope = scope.as_str();
            tracing::debug!(
                authority,
                digest,
                epoch = epoch.0,
                scope,
                evidence = ?ack.evidence,
                "oci write acknowledged"
            );
            Ok(())
        }
        WriteDurability::Pending | WriteDurability::Unavailable => {
            tracing::warn!(
                authority,
                digest,
                epoch = epoch.0,
                evidence = ?ack.evidence,
                durability = ?durability,
                "oci write durability unproven within the configured deadline"
            );
            Err(durability_pending())
        }
    }
}

pub(in crate::registry) async fn acknowledge_metadata(
    state: &ServingState,
    ack: MetadataAck<'_>,
) -> Result<(), Response> {
    let authority = crate::name::authority_key(ack.repo);
    let epoch = AuthorityEpoch(ack.epoch);
    let durability = state
        .confirm_metadata_write(CommittedMetadata::new(&authority, epoch, ack.commit))
        .await;
    let serial = ack.commit.serial();
    match durability {
        WriteDurability::Confirmed { .. } => {
            tracing::debug!(
                authority,
                epoch = epoch.0,
                serial,
                evidence = "journal-frontier",
                "oci metadata write acknowledged"
            );
            Ok(())
        }
        WriteDurability::Pending | WriteDurability::Unavailable => {
            tracing::warn!(
                authority,
                epoch = epoch.0,
                serial,
                evidence = "journal-frontier",
                durability = ?durability,
                "oci metadata write durability unproven within the configured deadline"
            );
            Err(durability_pending())
        }
    }
}

/// The retry response for a committed write the policy could not prove durable. It is a `503` a client
/// retries, and it names no leader, datacenter, or member count, so it leaks no topology.
///
/// The resolver already spent the configured write-acknowledgement budget, so the one-second delay
/// only keeps a retry from arriving before the receipts it waits on could plausibly land.
fn durability_pending() -> Response {
    let mut response = error_response(
        ErrorCode::Unavailable,
        "the configured write durability is not yet proven; retry the request",
    );
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, header::HeaderValue::from_static("1"));
    response
}
