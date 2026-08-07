//! Copying one verified blob across data centers: fetch it from a peer, then durably publish it locally.
//!
//! Which digests a data center owes a copy of, and which verified peer to pull each from, is the pure
//! selection [`plan_cross_dc_copy`](peryx_storage::meta::plan_cross_dc_copy) makes over the placement
//! ledger. This module is the transfer that selection resolves to: stream the whole blob under the
//! transport's byte cap, which digest-verifies it, then write it through
//! [`write_verified`](BlobStore::write_verified), which re-checks the digest and publishes with an
//! atomic filesystem operation. The orchestrator that mints the fence, bounds concurrency, and records
//! the resulting placement lives above this module.

use peryx_storage::blob::{BlobError, BlobStore, Digest};

use crate::blob::{BlobRequest, BlobTransport};
use crate::peer::TransportError;

/// A failure copying one blob to one target data center.
#[derive(Debug, thiserror::Error)]
pub enum CopyError {
    /// The source could not serve the blob's bytes.
    #[error("copy source could not serve the blob: {0}")]
    Fetch(#[source] TransportError),
    /// The target could not durably publish the fetched bytes.
    #[error("copy target could not publish the blob: {0}")]
    Publish(#[source] BlobError),
}

/// Copy `digest` from `source` to `target`, verifying and durably publishing the bytes.
///
/// Fetches the whole blob under the transport's byte cap, which a whole-blob fetch digest-verifies, then
/// writes it through [`write_verified`](BlobStore::write_verified), which re-checks the digest and
/// publishes with an atomic filesystem operation, so a target never exposes bytes that do not match the
/// digest and a partial write never reaches a served path.
///
/// # Errors
/// Returns [`CopyError::Fetch`] when the source cannot serve the blob and [`CopyError::Publish`] when the
/// target cannot durably write it.
pub async fn copy_blob_to_target(
    source: &(dyn BlobTransport + Send + Sync),
    target: &BlobStore,
    digest: &Digest,
) -> Result<(), CopyError> {
    let bytes = source
        .fetch_blob(BlobRequest {
            digest: digest.clone(),
            range: None,
        })
        .await
        .map_err(CopyError::Fetch)?;
    target.write_verified(&bytes, digest).map_err(CopyError::Publish)
}
