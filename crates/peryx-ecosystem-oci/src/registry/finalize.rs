//! Publishes admitted OCI blobs at their repository's home.
//!
//! A blob push retains a durable intent once its bytes are committed and before any repository
//! membership exists, so an upload whose publication is turned away - the authority moved mid-flight,
//! the metadata commit faulted, the process died between the two writes - leaves a record rather than
//! an orphan. The maintenance pass calls this once per tick: it reads the intents still pending,
//! rebuilds each upload's identity from its retained envelope, and republishes it under the current
//! home. An operator drain reaches the same publish one intent at a time through
//! [`finalize_retained`], naming the authority it drains.
//!
//! An intent leaves the pending set only after a metadata transaction has committed its effect. The
//! membership commit runs first and the settle follows it, so a pass interrupted between the two
//! leaves the intent pending and the next one republishes to the same state; nothing settles a write
//! that was never published.
//!
//! Every step revalidates rather than trusting the envelope. The index must still be configured, its
//! policy must still admit a push of that size, the content must still be readable here, and the
//! repository's home must still be this datacenter at an epoch the fence admits. Publication itself is
//! idempotent - the membership upsert, the session close, and the quota commit are the writes the
//! synchronous push performs - so a pass that stops after committing metadata and before settling its
//! intent republishes to the same state on the next tick.
//!
//! An intent whose content is gone, whose index no longer exists, or whose policy now refuses it can
//! never finalize here. Those are recorded as refusals, which drops them out of later batches once
//! they reach [`MAX_INTENT_REFUSALS`] so one unfinalizable head cannot occupy the batch and starve the
//! recoverable work behind it, and lets the reaper expire them. Every other skip is transient and
//! leaves the intent offered on the next pass, including a store or blob fault, which ends the whole
//! pass rather than settling anything on a store that is not answering.

use std::sync::Arc;

use peryx_driver::ServingState;
use peryx_driver::jobs::MAX_INTENT_REFUSALS;
use peryx_index::Index;
use peryx_policy::PolicyAction;
use peryx_storage::blob::Digest;
use peryx_storage::meta::{IntentPhase, OperationResult, StagedIntent};

use super::admission::{BLOB_INTENT_PREFIX, BlobIntent, PAYLOAD_VERSION};
use super::authority::{EpochCommit, claim_repository_home, commit_epoch};
use super::{ServeError, policy_blocks, policy_size_denial};
use crate::store;

/// Pending intents one maintenance-tick sweep reads, bounding a single pass's transaction fan-out; a
/// deeper backlog drains over successive ticks.
const SWEEP_BATCH: usize = 256;

/// Publish every admitted OCI blob whose intent is still pending and whose repository this node homes,
/// returning how many reached a terminal result.
pub(in crate::registry) async fn finalize_admitted(state: &Arc<ServingState>, journal: crate::outbox::Outbox) -> u64 {
    match sweep(state, journal).await {
        Ok(finalized) => finalized,
        Err(error) => {
            tracing::warn!(?error, "oci finalize sweep left its intents pending");
            0
        }
    }
}

async fn sweep(state: &Arc<ServingState>, journal: crate::outbox::Outbox) -> Result<u64, ServeError> {
    let pending = state.meta.list_pending_intents(SWEEP_BATCH, MAX_INTENT_REFUSALS)?;
    let mut finalized = 0_u64;
    for (key, intent) in &pending {
        // Another ecosystem's intents belong to its own finalizer.
        if key.starts_with(BLOB_INTENT_PREFIX) && finalize_one(state, key, intent, journal).await? {
            finalized += 1;
        }
    }
    Ok(finalized)
}

/// Publish the one retained OCI blob an operator drain named, reporting whether it settled.
///
/// The drain names the authority it is draining and the staging record names the authority the push was
/// admitted for. The two must agree, or this is another authority's write and publishing it here would
/// settle it under a home that never owned it. A key another ecosystem minted, a staging record that is
/// gone or unreadable, and a home that refuses the publish all report `false` and leave the intent
/// pending, so `false` never means the retained write is lost.
///
/// A drain settles every intent it homes, including ones this ecosystem's own sweep has given up on, so
/// the refusal count that bounds [`finalize_admitted`] does not gate this.
pub(in crate::registry) async fn finalize_retained(
    state: &Arc<ServingState>,
    journal: crate::outbox::Outbox,
    authority: &str,
    intent_key: &str,
) -> bool {
    if !intent_key.starts_with(BLOB_INTENT_PREFIX) {
        return false;
    }
    // A staging record that cannot be read is not evidence the write is gone, so it ends where a refused
    // publish ends, still pending.
    let Ok(Some(intent)) = state.meta.staged_intent(intent_key) else {
        return false;
    };
    if intent.authority != authority {
        return false;
    }
    match finalize_one(state, intent_key, &intent, journal).await {
        Ok(settled) => settled,
        Err(error) => {
            tracing::warn!(
                intent = intent_key,
                ?error,
                "retained oci blob write left pending for a later drain"
            );
            false
        }
    }
}

/// Publish the one blob retained under `key`, reporting whether it reached a terminal result.
async fn finalize_one(
    state: &Arc<ServingState>,
    key: &str,
    intent: &StagedIntent,
    journal: crate::outbox::Outbox,
) -> Result<bool, ServeError> {
    let Some(payload) = retained_envelope(key, intent) else {
        return Ok(false);
    };
    let Some(storage) = store::blob_digest(&payload.digest) else {
        return refuse(state, key, "the retained digest is not a supported blob address");
    };
    let Some(index) = publishable_index(state, &payload) else {
        return refuse(state, key, "no configured index still admits the retained push");
    };
    // The content is an unpublished orphan a crash left before its intent landed, or one content
    // cleanup has since reclaimed; either way no upload can finalize this here.
    if state.blobs.head(&storage).await?.is_none() {
        return refuse(state, key, "the retained content is not stored here");
    }
    publish(state, key, &payload, index, &storage, journal).await
}

/// The finalization envelope `intent` carries, or `None` when this build cannot read it. An envelope
/// written by a later build is left pending rather than guessed at, so an operator downgrade does not
/// settle a write it never published.
fn retained_envelope(key: &str, intent: &StagedIntent) -> Option<BlobIntent> {
    match serde_json::from_slice::<BlobIntent>(&intent.payload) {
        Ok(payload) if payload.version == PAYLOAD_VERSION => Some(payload),
        Ok(payload) => {
            tracing::error!(
                intent = key,
                version = payload.version,
                "retained oci blob intent was written by a later build"
            );
            None
        }
        Err(error) => {
            tracing::error!(intent = key, %error, "retained oci blob intent could not be decoded");
            None
        }
    }
}

/// The configured index whose store the retained push publishes into, or `None` when none still admits
/// it. Policy is re-evaluated here rather than trusted from admission, so a repository since closed to
/// uploads or a size limit since lowered refuses the push instead of publishing it.
fn publishable_index<'a>(state: &'a ServingState, payload: &BlobIntent) -> Option<&'a Index> {
    let index = state.indexes.iter().find(|index| index.name == payload.index)?;
    (!policy_blocks(index, PolicyAction::Upload, &payload.repo)
        && policy_size_denial(index, &payload.repo, payload.size).is_none())
    .then_some(index)
}

/// Commit the quota the push reserved, then publish its membership, close its upload session, and
/// journal the mutation under a live authority lease, settling the intent and the operation once the
/// metadata is committed.
///
/// The reservation is committed ahead of the membership rather than with it, because the reservation
/// the interrupted request took is the one this publication must charge, and a reservation an earlier
/// pass already committed commits again as a no-op. A pass that stops between the two leaves the
/// intent pending, so the next one republishes and settles.
async fn publish(
    state: &Arc<ServingState>,
    key: &str,
    payload: &BlobIntent,
    index: &Index,
    storage: &Digest,
    journal: crate::outbox::Outbox,
) -> Result<bool, ServeError> {
    let Ok(fence) = claim_repository_home(state, &payload.repo).await else {
        return Ok(false);
    };
    // The interrupted request claimed this id, but the ledger may have been reaped since, and a
    // terminal result cannot be recorded against an id nothing admitted. Re-claiming leaves an
    // existing record alone.
    state.claim_admitted_write(&payload.operation);
    if let Some(record) = &payload.reservation {
        state.meta.commit_quota_reservation(record.id)?;
    }
    let mutation = commit_epoch(state, &payload.repo, fence, |lease| {
        lease.guard()?;
        crate::quota::commit_blob_membership(
            &state.meta,
            &index.name,
            &payload.repo,
            &payload.digest,
            None,
            payload.session.as_deref(),
            journal,
        )
    })
    .await?;
    if matches!(mutation, EpochCommit::Fenced) {
        return Ok(false);
    }
    state.record_home_placement(storage.as_str(), payload.size, fence);
    state.meta.advance_intent(key, IntentPhase::Admitted, (state.clock)())?;
    state.finalize_admitted_write(&payload.operation, OperationResult::Published, b"");
    Ok(true)
}

/// Record that nothing this node could finalize was ever stored for `key`, so later batches skip it
/// and the reaper can expire it.
fn refuse(state: &ServingState, key: &str, reason: &str) -> Result<bool, ServeError> {
    tracing::debug!(intent = key, reason, "retained oci blob intent refused");
    state.meta.refuse_intent(key)?;
    Ok(false)
}

#[cfg(test)]
#[path = "../../tests/unit/registry/finalize/tests.rs"]
mod tests;
