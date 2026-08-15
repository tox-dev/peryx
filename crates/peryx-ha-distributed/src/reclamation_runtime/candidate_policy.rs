use peryx_ha::{
    ObservedFrontier, ReadyOutcome, ReclamationDecisionError, ReclamationStore, SelectOutcome,
    decide_reclamation_readiness, decide_reclamation_selection,
};
use peryx_identity::ArtifactDigest;
use peryx_storage::meta::{MetaError, MetaStore};

#[derive(Debug, thiserror::Error)]
pub enum ReclamationError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error(transparent)]
    Decision(#[from] ReclamationDecisionError),
}

/// # Errors
/// Returns a persistence or reclamation decision error.
pub fn select_reclamation_candidate(
    meta: &MetaStore,
    digest: &ArtifactDigest,
    referenced: bool,
    required_frontier: u64,
    fence: u64,
    now: i64,
) -> Result<SelectOutcome, ReclamationError> {
    loop {
        let snapshot = meta.reclamation_snapshot(digest)?;
        let outcome = decide_reclamation_selection(digest, &snapshot, referenced, required_frontier, fence, now)?;
        let Some(replacement) = outcome.replacement() else {
            return Ok(outcome);
        };
        if meta.compare_and_put_reclamation_tombstone(&snapshot, replacement)? {
            return Ok(outcome);
        }
    }
}

/// # Errors
/// Returns a persistence or reclamation decision error.
pub fn mark_reclamation_ready(
    meta: &MetaStore,
    digest: &ArtifactDigest,
    referenced: bool,
    observed: ObservedFrontier,
    fence: u64,
    now: i64,
) -> Result<ReadyOutcome, ReclamationError> {
    loop {
        let snapshot = meta.reclamation_snapshot(digest)?;
        let outcome = decide_reclamation_readiness(&snapshot, referenced, observed, fence, now)?;
        if snapshot.tombstone.as_ref() == Some(outcome.replacement())
            || meta.compare_and_put_reclamation_tombstone(&snapshot, outcome.replacement())?
        {
            return Ok(outcome);
        }
    }
}
