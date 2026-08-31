use std::num::NonZeroUsize;

use peryx_storage::blob::{BlobError, BlobStore, Digest};

use crate::blob::{BlobRequest, BlobTransport, ByteRange};
use crate::blob_pull::chunk_ranges;
use crate::peer::TransportError;

#[derive(Debug, thiserror::Error)]
pub enum CopyError {
    #[error("copy source could not serve the blob: {0}")]
    Fetch(#[source] TransportError),
    #[error("copy source served {actual} bytes for the {expected}-byte range at offset {offset}")]
    RangeLength {
        offset: usize,
        expected: usize,
        actual: usize,
    },
    #[error("copy target could not publish the blob: {0}")]
    Publish(#[source] BlobError),
}

/// Copies the blob one range at a time.
///
/// An artifact larger than the transport's per-fetch byte cap therefore still transfers. Each range is
/// written straight into an unpublished stage, and [`commit`](BlobStore::commit) verifies the whole
/// digest before publication; a failed range drops the stage without publishing anything.
///
/// `total_length` must come from the verified source placement rather than a peer advertisement, and
/// `range_bytes` must not exceed what `source` will serve in one response.
///
/// # Errors
/// Returns [`CopyError::Fetch`] when the source cannot serve a range, [`CopyError::RangeLength`] when it
/// serves a range of the wrong size, and [`CopyError::Publish`] when the target cannot stage, verify, or
/// publish the blob.
pub async fn copy_blob_to_target(
    source: &(dyn BlobTransport + Send + Sync),
    target: &BlobStore,
    digest: &Digest,
    total_length: usize,
    range_bytes: NonZeroUsize,
) -> Result<(), CopyError> {
    let mut pending = target.begin().map_err(CopyError::Publish)?;
    for range in chunk_ranges(total_length, range_bytes.get()) {
        let bytes = fetch_range(source, digest, range).await?;
        pending.write(&bytes).map_err(CopyError::Publish)?;
    }
    target.commit(pending, digest).map_err(CopyError::Publish)?;
    Ok(())
}

async fn fetch_range(
    source: &(dyn BlobTransport + Send + Sync),
    digest: &Digest,
    range: ByteRange,
) -> Result<Vec<u8>, CopyError> {
    let bytes = source
        .fetch_blob(BlobRequest {
            digest: digest.clone(),
            range: Some(range),
        })
        .await
        .map_err(CopyError::Fetch)?;
    if bytes.len() == range.length {
        Ok(bytes)
    } else {
        Err(CopyError::RangeLength {
            offset: range.offset,
            expected: range.length,
            actual: bytes.len(),
        })
    }
}
