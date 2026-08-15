use peryx_ha::ReclamationStore;
use peryx_identity::ArtifactDigest;
use peryx_storage::meta::MetaStore;

use super::candidate_policy::ReclamationError;

/// # Errors
/// Returns a persistence error or rejects an older ownership fence.
pub fn forget_reclamation_tombstone(
    meta: &MetaStore,
    digest: &ArtifactDigest,
    fence: u64,
) -> Result<bool, ReclamationError> {
    loop {
        let Some(tombstone) = meta.reclamation_tombstone(digest)? else {
            return Ok(false);
        };
        tombstone.validate_fence(fence)?;
        if meta.compare_and_remove_reclamation_tombstone(&tombstone)? {
            return Ok(true);
        }
    }
}
