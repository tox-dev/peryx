//! Whole-blob verification records chunk digests by content address so later ranged reads can verify and
//! forward each chunk. Missing entries require whole-blob staging; entries do not identify placements.

use peryx_identity::ArtifactDigest;

use super::{BLOB_CHUNK_DIGEST, MetaError, MetaStore, open_optional_table};
use crate::blob::ChunkedDigest;

impl MetaStore {
    /// Replaces any prior entry; callers must supply digests computed from whole-verified bytes.
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

    /// Returns `None` until a node catalogs the blob's chunks.
    ///
    /// # Errors
    /// Returns a store error when the row cannot be read or decoded.
    pub fn blob_chunk_digest(&self, digest: &ArtifactDigest) -> Result<Option<ChunkedDigest>, MetaError> {
        let txn = self.db.begin_read()?;
        let Some(table) = open_optional_table(&txn, BLOB_CHUNK_DIGEST)? else {
            return Ok(None);
        };
        Ok(table
            .get(digest.canonical().as_str())?
            .map(|value| serde_json::from_slice(value.value()))
            .transpose()?)
    }
}

#[cfg(test)]
#[path = "../../tests/unit/meta/blob_chunk_digest/tests.rs"]
mod tests;
