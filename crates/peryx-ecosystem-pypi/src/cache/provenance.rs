//! Serving PEP 740 provenance: resolve a distribution's stored provenance blob from its artifact
//! digest, the same digest-keyed lookup the `.metadata` sibling uses.

use std::sync::Arc;

use bytes::Bytes;
use peryx_driver::state::ServingState;
use peryx_storage::blob::Digest;

use crate::store::PypiStore as _;

use super::CacheError;

/// The largest provenance blob peryx serves. An upload's `attestations` field is capped at 1 MiB, so
/// the wrapped provenance object stays comfortably under this ceiling; it bounds the read all the same.
const MAX_PROVENANCE_BYTES: u64 = 2 * 1024 * 1024;

/// The provenance object bytes for an artifact digest, read from the blob its upload staged.
///
/// # Errors
/// Returns [`CacheError::FileNotFound`] when the artifact carries no provenance sibling or its blob is
/// gone, or another error on a store or blob failure.
pub async fn provenance_bytes(state: &Arc<ServingState>, artifact_digest: &Digest) -> Result<Bytes, CacheError> {
    let (provenance_hex, _size) = state
        .meta
        .get_provenance(artifact_digest.as_str())?
        .ok_or(CacheError::FileNotFound)?;
    let provenance_digest = Digest::from_hex(&provenance_hex).ok_or(CacheError::FileNotFound)?;
    if state.blobs.head(&provenance_digest).await?.is_none() {
        return Err(CacheError::FileNotFound);
    }
    Ok(Bytes::from(
        state.blobs.read_bytes(&provenance_digest, MAX_PROVENANCE_BYTES).await?,
    ))
}

#[cfg(test)]
mod tests {
    use peryx_storage::blob::BlobStore;
    use peryx_storage::meta::MetaStore;

    use super::*;

    fn test_state() -> (tempfile::TempDir, Arc<ServingState>) {
        let dir = tempfile::tempdir().unwrap();
        let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
        let blobs = BlobStore::new(dir.path().join("blobs"));
        (dir, peryx_driver::AppState::new(meta, blobs, 60, Vec::new()).serving)
    }

    #[tokio::test]
    async fn test_provenance_bytes_reads_the_stored_blob() {
        let (_dir, state) = test_state();
        let artifact = Digest::of(b"artifact");
        let document = br#"{"version":1,"attestation_bundles":[]}"#;
        let provenance = state.blobs.put_bytes(document).await.unwrap();
        state
            .meta
            .put_provenance(artifact.as_str(), provenance.as_str(), document.len() as u64)
            .unwrap();

        let bytes = provenance_bytes(&state, &artifact).await.unwrap();

        assert_eq!(&bytes[..], document);
    }

    #[tokio::test]
    async fn test_provenance_bytes_is_not_found_without_a_record() {
        let (_dir, state) = test_state();

        assert!(matches!(
            provenance_bytes(&state, &Digest::of(b"absent")).await,
            Err(CacheError::FileNotFound)
        ));
    }

    #[tokio::test]
    async fn test_provenance_bytes_is_not_found_when_the_blob_is_gone() {
        let (_dir, state) = test_state();
        let artifact = Digest::of(b"artifact");
        state
            .meta
            .put_provenance(artifact.as_str(), &"a".repeat(64), 16)
            .unwrap();

        assert!(matches!(
            provenance_bytes(&state, &artifact).await,
            Err(CacheError::FileNotFound)
        ));
    }
}
