//! The ingress DC's admission state for staged write intents.
//!
//! Before a home DC can finalize a write, the ingress DC that received it retains the upload near the
//! client as an *intent*. A client retries an interrupted upload, so admission must be idempotent: the
//! same idempotency key from the same tenant and authority names the same intended write, and staging
//! it again returns the first admission rather than a second slot. Reusing that key for different
//! content is a conflict, not a silent overwrite, so a retry cannot swap the bytes a home DC will
//! finalize. A bounded retention buffer refuses a new intent past its limit rather than growing without
//! end.
//!
//! Each admitted intent moves through a monotonic lifecycle - [`Pending`](IntentState::Pending) at the
//! ingress DC, [`Admitted`](IntentState::Admitted) once the home DC finalizes it, then
//! [`Expired`](IntentState::Expired) when its retention window elapses - and a transition only advances
//! it, so a replayed or reordered lifecycle event can never move a settled intent backward.
//!
//! This module is the pure admission and lifecycle state. Persisting intents durably, waiting for
//! same-DC byte durability, recovering them across a restart, and streaming request bodies are deferred
//! wiring.

use std::collections::HashMap;
use std::num::NonZeroUsize;

/// The client-scoped identity that deduplicates a retried admission.
///
/// The same `idempotency_key` from the same `tenant` and `authority_key` names the same intended write;
/// scoping the key to the tenant and authority keeps one client's key from colliding with another's.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IntentKey {
    pub tenant: String,
    pub authority_key: String,
    pub idempotency_key: String,
}

/// A staged ingress write awaiting home-DC finalization: its client-scoped identity, the content it
/// commits to as a digest and byte size, and the provenance the finalization and drain paths need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngressIntent {
    pub key: IntentKey,
    pub ecosystem: peryx_core::Ecosystem,
    pub digest: String,
    pub size: u64,
    pub ingress_dc: String,
    pub operation_id: String,
}

/// Where a staged intent sits in its lifecycle.
///
/// The variants are declared in advancing order, so the derived ordering ranks `Pending` below
/// `Admitted` below `Expired`, and a transition only moves forward, never back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum IntentState {
    /// Admitted at the ingress DC and retained, awaiting home-DC finalization.
    Pending,
    /// The home DC finalized the write; the intent is settled.
    Admitted,
    /// The retention window elapsed before finalization; the intent is eligible for reclamation.
    Expired,
}

/// The verdict of staging an intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageOutcome {
    /// A new distinct intent was admitted and retained as [`Pending`](IntentState::Pending).
    Admitted,
    /// The idempotency key was already admitted for the same content, so the first admission stands and
    /// no second intent is retained.
    Duplicate,
    /// The idempotency key was already admitted for different content, so it is refused rather than
    /// overwriting the bytes a home DC will finalize.
    Conflict,
    /// The retention buffer already holds its limit, so a new intent is refused.
    RejectedOverLimit,
}

/// Whether a lifecycle transition advanced an intent or was dropped as a replay or a move backward.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionOutcome {
    /// The target state was later than the intent's current state, so the intent advanced to it.
    Advanced,
    /// The target was the current state or earlier, or no such intent exists, so the state stands.
    Ignored,
}

#[derive(Debug, Clone)]
struct Staged {
    intent: IngressIntent,
    state: IntentState,
}

/// The ingress DC's retained write intents, admitted idempotently and advanced monotonically, bounded by
/// a retention limit.
#[derive(Debug)]
pub struct IntentLedger {
    limit: NonZeroUsize,
    intents: HashMap<IntentKey, Staged>,
}

impl IntentLedger {
    /// A ledger that retains at most `limit` intents before refusing new ones.
    #[must_use]
    pub fn new(limit: NonZeroUsize) -> Self {
        Self {
            limit,
            intents: HashMap::new(),
        }
    }

    /// Admit `intent` into the retention buffer.
    ///
    /// A key already staged for the same digest and size is a [`Duplicate`](StageOutcome::Duplicate) and
    /// changes nothing; the same key for different content is a [`Conflict`](StageOutcome::Conflict) and
    /// leaves the first intent intact; a new key past the retention limit is
    /// [`RejectedOverLimit`](StageOutcome::RejectedOverLimit). Only a new key within the limit is
    /// [`Admitted`](StageOutcome::Admitted), retained as [`Pending`](IntentState::Pending).
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

    /// Advance the intent under `key` to `to`, returning whether it moved.
    ///
    /// The transition applies only when `to` is later than the intent's current state, so a replayed or
    /// reordered event, or one naming an unknown intent, leaves the lifecycle unchanged.
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

    /// The lifecycle state of the intent under `key`, or `None` when none is retained.
    #[must_use]
    pub fn state(&self, key: &IntentKey) -> Option<IntentState> {
        self.intents.get(key).map(|staged| staged.state)
    }

    /// The intent retained under `key`, or `None` when none is.
    #[must_use]
    pub fn get(&self, key: &IntentKey) -> Option<&IngressIntent> {
        self.intents.get(key).map(|staged| &staged.intent)
    }

    /// How many intents the ledger currently retains.
    #[must_use]
    pub fn len(&self) -> usize {
        self.intents.len()
    }

    /// Whether the ledger retains no intents.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.intents.is_empty()
    }
}
