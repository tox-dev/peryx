use peryx_identity::{ArtifactDigest, RevocationReason, UserId};
use peryx_storage::meta::MetaError;

use super::app::ServingState;
use crate::revocations::{LiftRevocationOutcome, PutRevocationError, PutRevocationOutcome};

/// A digest revocation decides which distributions an ecosystem may describe, so the derived search view
/// is retired alongside the decision rather than by whichever transport happened to change it.
///
/// The whole view is retired, not one record: a revocation names a digest, and no index maps a digest back
/// to the projects that publish it. Retirement is lazy, so an operator action that changes nothing pays
/// nothing, and one that does pays a single re-derivation on the next query.
impl ServingState {
    /// # Errors
    /// Returns a reason conflict or store error from the transaction.
    pub fn put_digest_revocation(
        &self,
        digest: &ArtifactDigest,
        reason: &RevocationReason,
        actor: &UserId,
    ) -> Result<PutRevocationOutcome, PutRevocationError> {
        let outcome = self.revocations.put(digest, reason, actor, (self.clock)())?;
        if !matches!(outcome, PutRevocationOutcome::Unchanged(_)) {
            self.bump_search_epoch();
        }
        Ok(outcome)
    }

    /// # Errors
    /// Returns a store error from the transaction.
    pub fn lift_digest_revocation(
        &self,
        digest: &ArtifactDigest,
        actor: &UserId,
    ) -> Result<Option<LiftRevocationOutcome>, MetaError> {
        let outcome = self.revocations.lift(digest, actor, (self.clock)())?;
        if matches!(outcome, Some(LiftRevocationOutcome::Lifted(_))) {
            self.bump_search_epoch();
        }
        Ok(outcome)
    }
}
