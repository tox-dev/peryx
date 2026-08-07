//! The home-side finalize sweep: turn this node's admitted-but-pending `PyPI` ingress intents into
//! published releases.
//!
//! An upload is admitted wherever a client reaches, staged as a [pending ingress
//! intent](peryx_storage::meta::IntentPhase::Pending) whose bytes are already durable at the ingress
//! datacenter. Its authority's home is the one node that turns it into a release. The scheduled
//! maintenance pass calls this once per tick: it reads the pending intents, reconstructs each admitted
//! upload's identity from the durable staging record, and drives it through
//! [`finalize_admitted_upload`], whose per-authority fence turns away an intent this node does not
//! home. It is the crash-recovery path for an admission whose file rows were written but whose intent a
//! restart left pending, so the intent's advance, the operation's terminal outcome, and the outbox
//! entry commit together on the next pass.

use std::sync::Arc;

use peryx_driver::state::ServingState;
use peryx_identity::{Action, Principal, authorize};
use peryx_index::Index;
use peryx_storage::meta::StagedIntent;

use super::finalize::{FinalizeDescriptor, finalize_admitted_upload};
use crate::filename::distribution_version_segment;
use crate::store::upload_key;

/// Pending intents one maintenance-tick sweep reads and attempts to finalize, bounding a single pass's
/// transaction fan-out; a deeper backlog drains over successive ticks.
const SWEEP_BATCH: usize = 256;

/// The prefix `PyPI` admission mints its intent keys under: `pypi:{route}:{authority}:{filename}`.
const PYPI_INTENT_PREFIX: &str = "pypi:";

/// How long a finalize's terminal operation outcome is retained before the write-ledger reaper may
/// prune it, matching the retention the synchronous upload path stamps.
const OUTCOME_RETENTION_SECS: i64 = 24 * 3600;

/// Finalize every `PyPI` upload this node admitted whose intent is still pending, returning how many
/// finalized. A read failure, a non-`PyPI` intent, an upload whose rows are not stored here, an index
/// with no live write token, or a fence rejection each skips the intent rather than failing the pass,
/// so one unfinalizable intent never blocks the backlog behind it.
pub async fn finalize_admitted(state: &Arc<ServingState>) -> u64 {
    let pending = match state.meta.list_pending_intents(SWEEP_BATCH) {
        Ok(pending) => pending,
        Err(error) => {
            tracing::error!(%error, "finalize sweep could not read pending intents");
            return 0;
        }
    };
    let mut finalized = 0_u64;
    for (key, intent) in &pending {
        if finalize_one(state, key, intent).await {
            finalized += 1;
        }
    }
    finalized
}

/// Finalize the one intent staged under `key`, returning whether it reached a terminal outcome. Returns
/// `false` for an intent this node cannot finalize - not `PyPI`, no stored rows, no write token, or fenced
/// out - so the caller counts only the intents it advanced.
async fn finalize_one(state: &Arc<ServingState>, key: &str, intent: &StagedIntent) -> bool {
    let Some(filename) = pypi_filename(key) else {
        return false;
    };
    let Some((index, record)) = locate(state, &intent.authority, filename) else {
        return false;
    };
    let Some(principal) = write_principal(index, &intent.authority) else {
        return false;
    };
    let operation = format!("{key}:{}", intent.digest);
    let descriptor = FinalizeDescriptor {
        operation: &operation,
        authority: &intent.authority,
        principal: &principal,
        index_name: &index.name,
        normalized: &intent.authority,
        display: &intent.authority,
        filename,
        artifact_sha256: &intent.digest,
        artifact_size: intent.size,
        record: &record,
        version: distribution_version_segment(filename).unwrap_or_default(),
        submitted_at_unix: intent.updated_at_unix,
        expiry_unix: Some((state.clock)() + OUTCOME_RETENTION_SECS),
    };
    match finalize_admitted_upload(state, key, &descriptor).await {
        Ok(_) => true,
        Err(error) => {
            tracing::debug!(intent = key, ?error, "intent left pending for a later finalize pass");
            false
        }
    }
}

/// The distribution filename an intent key carries, or `None` when the key is not a `PyPI` intent. A `PyPI`
/// key is `pypi:{route}:{authority}:{filename}`; the authority is read from the staging record, so only
/// the filename is parsed back out here.
fn pypi_filename(key: &str) -> Option<&str> {
    let rest = key.strip_prefix(PYPI_INTENT_PREFIX)?;
    let mut parts = rest.splitn(3, ':');
    let _route = parts.next()?;
    let _authority = parts.next()?;
    parts.next()
}

/// The configured index whose store holds this upload's rows, paired with its serialized record. The
/// bytes were written where the upload was admitted, so a home finalize reads the record back to
/// confirm the write landed and to re-publish it idempotently; an intent whose rows are not stored here
/// has nothing to finalize.
fn locate<'a>(state: &'a ServingState, authority: &str, filename: &str) -> Option<(&'a Index, Vec<u8>)> {
    state.indexes.iter().find_map(|index| {
        let record = state
            .meta
            .get_driver_value(&upload_key(&index.name, authority, filename))
            .ok()??;
        Some((index, record))
    })
}

/// A principal an index still authorizes to write `project`, standing in for the uploader at
/// finalization so authorization is re-checked against the current ACL rather than trusted from
/// admission. `None` when no token grants the write, which fails the finalize closed.
fn write_principal(index: &Index, project: &str) -> Option<Principal> {
    index.acl.tokens.iter().find_map(|token| {
        let principal = Principal::Named {
            subject: token.name.clone(),
        };
        authorize(&principal, &index.acl, Some(project), Action::Write)
            .is_ok()
            .then_some(principal)
    })
}
