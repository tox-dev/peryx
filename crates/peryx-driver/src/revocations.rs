use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use moka::sync::Cache;
use peryx_events::security::Event;
use peryx_identity::{ArtifactDigest, DigestDecision, RevocationReason, UserId};
pub use peryx_storage::meta::{
    DigestRevocation, DigestRevocationQuery, DigestRevocationQueryError, DigestRevocationStatus, LiftRevocationOutcome,
    PutRevocationError, PutRevocationOutcome,
};
use peryx_storage::meta::{DigestRevocationPage, DigestRevocationState, MetaError, MetaStore};

const CACHE_CAPACITY: u64 = 16_384;
const ACTIVE_UNKNOWN: u8 = 0;
const ACTIVE_NONE: u8 = 1;
const ACTIVE_SOME: u8 = 2;
/// Maximum age shared serving and HTTP caches may retain a digest decision.
pub const DECISION_CACHE_TTL_SECS: u64 = 60;

/// Shared digest-revocation operations for HTTP management and future download enforcement.
#[derive(Clone)]
pub struct RevocationService {
    store: MetaStore,
    decisions: Cache<ArtifactDigest, DigestDecision>,
    cache_gate: Arc<RwLock<()>>,
    active: Arc<AtomicU8>,
}

impl RevocationService {
    #[must_use]
    pub fn new(store: MetaStore) -> Self {
        Self {
            store,
            decisions: Cache::builder()
                .max_capacity(CACHE_CAPACITY)
                .time_to_live(Duration::from_secs(DECISION_CACHE_TTL_SECS))
                .build(),
            cache_gate: Arc::new(RwLock::new(())),
            active: Arc::new(AtomicU8::new(ACTIVE_UNKNOWN)),
        }
    }

    /// # Errors
    /// Returns a store error when the authoritative row cannot be read.
    pub fn decision(&self, digest: &ArtifactDigest) -> Result<DigestDecision, MetaError> {
        if !self.has_active()? {
            return Ok(DigestDecision::Clear);
        }
        if let Some(decision) = self.decisions.get(digest) {
            return Ok(decision);
        }
        let _guard = self
            .cache_gate
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let decision = self
            .store
            .digest_revocation(digest)?
            .map_or(DigestDecision::Clear, |record| match record.state {
                DigestRevocationState::Active => DigestDecision::Revoked,
                DigestRevocationState::Lifted { .. } => DigestDecision::Clear,
            });
        self.decisions.insert(digest.clone(), decision);
        Ok(decision)
    }

    /// # Errors
    /// Returns a reason conflict or store error from the transaction.
    pub fn put(
        &self,
        digest: &ArtifactDigest,
        reason: &RevocationReason,
        actor: &UserId,
        now: i64,
    ) -> Result<PutRevocationOutcome, PutRevocationError> {
        let guard = self
            .cache_gate
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.active.store(ACTIVE_UNKNOWN, Ordering::Release);
        let outcome = self.store.put_digest_revocation(digest, reason, actor, now)?;
        self.decisions.invalidate(digest);
        drop(guard);
        emit_change(
            "digest_revocation_put",
            digest,
            actor,
            !matches!(outcome, PutRevocationOutcome::Unchanged(_)),
        );
        Ok(outcome)
    }

    /// # Errors
    /// Returns a store error from the transaction.
    pub fn lift(
        &self,
        digest: &ArtifactDigest,
        actor: &UserId,
        now: i64,
    ) -> Result<Option<LiftRevocationOutcome>, MetaError> {
        let guard = self
            .cache_gate
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.active.store(ACTIVE_UNKNOWN, Ordering::Release);
        let outcome = self.store.lift_digest_revocation(digest, actor, now)?;
        self.decisions.invalidate(digest);
        drop(guard);
        emit_change(
            "digest_revocation_lift",
            digest,
            actor,
            matches!(outcome, Some(LiftRevocationOutcome::Lifted(_))),
        );
        Ok(outcome)
    }

    /// # Errors
    /// Returns a store error when the row cannot be read.
    pub fn inspect(&self, digest: &ArtifactDigest) -> Result<Option<DigestRevocation>, MetaError> {
        self.store.digest_revocation(digest)
    }

    /// # Errors
    /// Returns a store error when the transactional active index cannot be read.
    pub fn has_active(&self) -> Result<bool, MetaError> {
        let mut state = self.active.load(Ordering::Acquire);
        if state == ACTIVE_UNKNOWN {
            let _guard = self
                .cache_gate
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let active = self.store.has_active_digest_revocation()?;
            state = if active { ACTIVE_SOME } else { ACTIVE_NONE };
            self.active.store(state, Ordering::Release);
        }
        Ok(state == ACTIVE_SOME)
    }

    /// # Errors
    /// Returns a query or store error.
    pub fn list(&self, query: &DigestRevocationQuery) -> Result<DigestRevocationPage, DigestRevocationQueryError> {
        self.store.query_digest_revocations(query)
    }
}

fn emit_change(action: &'static str, digest: &ArtifactDigest, actor: &UserId, changed: bool) {
    let digest = digest.canonical();
    Event::new(action, "success")
        .actor(Some(actor.as_str()))
        .digest(Some(&digest))
        .changed(changed)
        .emit();
}
