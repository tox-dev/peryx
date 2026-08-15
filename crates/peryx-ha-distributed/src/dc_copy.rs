use peryx_storage::blob::{BlobError, BlobStore, Digest};

use crate::blob::{BlobRequest, BlobTransport};
use crate::peer::TransportError;

#[derive(Debug, thiserror::Error)]
pub enum CopyError {
    #[error("copy source could not serve the blob: {0}")]
    Fetch(#[source] TransportError),
    #[error("copy target could not publish the blob: {0}")]
    Publish(#[source] BlobError),
}

/// Fetches the whole blob under the transport byte cap, then uses
/// [`write_verified`](BlobStore::write_verified) for a second digest check and atomic publication.
///
/// # Errors
/// Returns [`CopyError::Fetch`] when the source cannot serve the blob and [`CopyError::Publish`] when the
/// target cannot verify or publish it.
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
