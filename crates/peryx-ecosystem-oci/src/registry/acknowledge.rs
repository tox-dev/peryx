//! An OCI push answers `201 Created` only once the configured `[availability.write_ack]` policy has
//! proven the write durable.
//!
//! The distribution specification fixes the client-visible success codes; it says nothing about how
//! many copies stand behind them. Peryx's acknowledgement contract does, and it lives in the shared
//! availability plane: [`ServingState::confirm_blob_write`] weighs the bytes this node committed
//! against the peer receipts and the metadata frontier the policy demands. Every terminal OCI blob
//! write routes its committed result through that one resolver, the same one the package upload path
//! uses, so both success codes mean the same thing about how much of the cluster holds the content.
//!
//! A write the policy cannot prove durable inside its deadline is not a failure: the content is
//! committed and the operation stays pending, so the client retries the identical request and the
//! content-addressed commit and membership upsert replay without a second effect.

use axum::http::header;
use axum::response::Response;
use peryx_driver::ServingState;
use peryx_ha::{AuthorityEpoch, CommittedBlob, WriteDurability};
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
