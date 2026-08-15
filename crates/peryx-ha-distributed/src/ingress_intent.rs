//! Tenant, authority, and idempotency key identify an intent. Reuse with different content is a conflict;
//! capacity rejects new keys. Lifecycle state moves forward, so replayed events cannot move settled
//! intents backward.

use std::collections::HashMap;
use std::num::NonZeroUsize;

/// Scopes idempotency keys by tenant and authority to prevent cross-client collisions.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IntentKey {
    pub tenant: String,
    pub authority_key: String,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngressIntent {
    pub key: IntentKey,
    pub ecosystem: peryx_core::Ecosystem,
    pub digest: String,
    pub size: u64,
    pub ingress_dc: String,
    pub operation_id: String,
}

/// Variant order defines the allowed lifecycle progression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum IntentState {
    Pending,
    Admitted,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageOutcome {
    Admitted,
    Duplicate,
    Conflict,
    RejectedOverLimit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionOutcome {
    Advanced,
    Ignored,
}

#[derive(Debug, Clone)]
struct Staged {
    intent: IngressIntent,
    state: IntentState,
}

#[derive(Debug)]
pub struct IntentLedger {
    limit: NonZeroUsize,
    intents: HashMap<IntentKey, Staged>,
}

impl IntentLedger {
    /// Rejects new keys after retaining `limit` intents.
    #[must_use]
    pub fn new(limit: NonZeroUsize) -> Self {
        Self {
            limit,
            intents: HashMap::new(),
        }
    }

    /// Preserves the first intent for a key. Equal content is a duplicate; different content conflicts.
    pub fn stage(&mut self, intent: IngressIntent) -> StageOutcome {
        if let Some(staged) = self.intents.get(&intent.key) {
            return if staged.intent.digest == intent.digest && staged.intent.size == intent.size {
                StageOutcome::Duplicate
            } else {
                StageOutcome::Conflict
            };
        }
        if self.intents.len() >= self.limit.get() {
            return StageOutcome::RejectedOverLimit;
        }
        self.intents.insert(
            intent.key.clone(),
            Staged {
                intent,
                state: IntentState::Pending,
            },
        );
        StageOutcome::Admitted
    }

    /// Advances to a later state; stale, duplicate, and unknown transitions have no effect.
    pub fn advance(&mut self, key: &IntentKey, to: IntentState) -> TransitionOutcome {
        let Some(staged) = self.intents.get_mut(key) else {
            return TransitionOutcome::Ignored;
        };
        if to <= staged.state {
            return TransitionOutcome::Ignored;
        }
        staged.state = to;
        TransitionOutcome::Advanced
    }

    #[must_use]
    pub fn state(&self, key: &IntentKey) -> Option<IntentState> {
        self.intents.get(key).map(|staged| staged.state)
    }

    #[must_use]
    pub fn get(&self, key: &IntentKey) -> Option<&IngressIntent> {
        self.intents.get(key).map(|staged| &staged.intent)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.intents.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.intents.is_empty()
    }
}
