//! The per-digest chunked-digest catalog.
//!
//! A blob's whole-blob digest verifies it only once every byte is reassembled and hashed, so an
//! incremental fetch of a large blob cannot trust - and cannot forward - a chunk until the whole arrives.
//! This catalog records the [`ChunkedDigest`] of a blob when a node whole-verifies its bytes, keyed by the
//! content digest, so a later incremental read-through verifies each chunk against its own recorded digest
//! and stages it before the rest of the blob is drawn.
//!
//! The entry is content-keyed, not placement-keyed: it says nothing about where a blob lives, only what the
//! sha256 of each of its fixed spans is. A read-through that has the entry may stream chunk-by-chunk; one
//! that does not falls back to whole-blob staging, and records the entry from that whole-verified pull so
//! the next fetch can stream.

use peryx_identity::ArtifactDigest;

use super::{BLOB_CHUNK_DIGEST, MetaError, MetaStore};
use crate::blob::ChunkedDigest;

impl MetaStore {
    /// Record the [`ChunkedDigest`] of `digest`, computed from whole-verified bytes, overwriting any prior
    /// entry for the same content.
    ///
    /// # Errors
    /// Returns a store error when the value cannot be encoded or the write cannot commit.
    pub fn put_blob_chunk_digest(&self, digest: &ArtifactDigest, chunked: &ChunkedDigest) -> Result<(), MetaError> {
        let value = serde_json::to_vec(chunked).map_err(MetaError::from)?;
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(BLOB_CHUNK_DIGEST)?;
            table.insert(digest.canonical().as_str(), value.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// The recorded [`ChunkedDigest`] of `digest`, or `None` when no node has catalogued its chunks yet.
    ///
    /// # Errors
    /// Returns a store error when the row cannot be read or decoded.
    pub fn blob_chunk_digest(&self, digest: &ArtifactDigest) -> Result<Option<ChunkedDigest>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(BLOB_CHUNK_DIGEST)?;
        Ok(table
            .get(digest.canonical().as_str())?
            .map(|value| serde_json::from_slice(value.value()))
            .transpose()?)
    }
}

#[cfg(test)]
mod tests;
