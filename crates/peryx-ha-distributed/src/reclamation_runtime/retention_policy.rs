use peryx_ha::{ReclamationProgress, ReclamationStore};
use peryx_storage::meta::{MetaError, MetaStore};

/// # Errors
/// Returns a persistence error.
pub fn prune_skipped_reclamation_tombstones(meta: &MetaStore, limit: usize) -> Result<usize, MetaError> {
    let tombstones = meta.reclamation_tombstones()?;
    let mut removed = 0;
    for tombstone in tombstones.iter().filter(|tombstone| tombstone.is_skipped()) {
        if removed == limit {
            break;
        }
        if meta.compare_and_remove_reclamation_tombstone(tombstone)? {
            removed += 1;
        }
    }
    Ok(removed)
}

/// # Errors
/// Returns a persistence error.
pub fn reclamation_progress(meta: &MetaStore) -> Result<ReclamationProgress, MetaError> {
    let tombstones = meta.reclamation_tombstones()?;
    Ok(ReclamationProgress::from_tombstones(&tombstones))
}
