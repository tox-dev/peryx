//! Replicated visibility projection and durable frontier tracking.
//!
//! Followers persist projection state before advancing the advertised frontier. A failed save leaves
//! both unchanged, preventing advertised coverage from exceeding durable apply state.

use peryx_ha::VisibilitySnapshotStore;
use serde::{Deserialize, Serialize};

use crate::envelope::{AuthorityEpoch, OperationEnvelope, OperationKind};
use crate::protocol::Change;
use crate::visibility::{
    ApplyEffect, ArtifactId, Frontier, OpOrder, SnapshotError, Visibility, VisibilityAction, VisibilityOp,
    VisibilityState,
};

/// Decode rejects other schemas to avoid applying a transition under the wrong identity.
pub const VISIBILITY_CHANGE_SCHEMA: u32 = 1;

const VISIBILITY_FEED_SCHEMA: u32 = 1;

#[derive(Serialize, Deserialize)]
struct EncodedOp {
    schema: u32,
    artifact: ArtifactId,
    action: VisibilityAction,
    order: OpOrder,
}

fn encode_op(op: &VisibilityOp) -> Vec<u8> {
    let encoded = EncodedOp {
        schema: VISIBILITY_CHANGE_SCHEMA,
        artifact: op.artifact.clone(),
        action: op.action,
        order: op.order,
    };
    serde_json::to_vec(&encoded).expect("a visibility operation always serializes to JSON")
}

/// Uses the operation serial so visibility shares the journal's mutation order.
#[must_use]
pub fn visibility_change(op: &VisibilityOp) -> Change {
    Change {
        serial: op.order.serial,
        event: encode_op(op),
        metadata: Vec::new(),
        blobs: Vec::new(),
    }
}

#[must_use]
pub fn visibility_envelope(source: impl Into<String>, op: &VisibilityOp) -> OperationEnvelope {
    OperationEnvelope::current(
        source,
        AuthorityEpoch(op.order.epoch),
        OperationKind::Visibility,
        visibility_change(op),
    )
}

#[derive(Debug, thiserror::Error)]
pub enum VisibilityFeedError {
    #[error("visibility change payload is malformed: {0}")]
    Malformed(#[source] serde_json::Error),
    #[error("visibility change payload schema {found} is not the {expected} this build applies")]
    UnsupportedSchema { expected: u32, found: u32 },
    #[error(
        "visibility op identity (epoch {op_epoch} serial {op_serial}) disagrees with its envelope \
         (epoch {envelope_epoch} serial {envelope_serial})"
    )]
    IdentityMismatch {
        envelope_epoch: u64,
        envelope_serial: u64,
        op_epoch: u64,
        op_serial: u64,
    },
}

/// Returns `None` for non-visibility envelopes. The payload epoch and serial must match the envelope.
///
/// # Errors
/// Returns [`VisibilityFeedError`] for malformed event bytes, a payload schema this build does not
/// apply, or an operation whose epoch or serial disagrees with its envelope.
pub fn decode_visibility_op(envelope: &OperationEnvelope) -> Result<Option<VisibilityOp>, VisibilityFeedError> {
    if envelope.kind != OperationKind::Visibility {
        return Ok(None);
    }
    let encoded: EncodedOp = serde_json::from_slice(&envelope.change.event).map_err(VisibilityFeedError::Malformed)?;
    if encoded.schema != VISIBILITY_CHANGE_SCHEMA {
        return Err(VisibilityFeedError::UnsupportedSchema {
            expected: VISIBILITY_CHANGE_SCHEMA,
            found: encoded.schema,
        });
    }
    let op = VisibilityOp {
        artifact: encoded.artifact,
        action: encoded.action,
        order: encoded.order,
    };
    if op.order.serial != envelope.change.serial || op.order.epoch != envelope.epoch.0 {
        return Err(VisibilityFeedError::IdentityMismatch {
            envelope_epoch: envelope.epoch.0,
            envelope_serial: envelope.change.serial,
            op_epoch: op.order.epoch,
            op_serial: op.order.serial,
        });
    }
    Ok(Some(op))
}

#[derive(Serialize, Deserialize)]
struct ProjectionSnapshot {
    schema: u32,
    advertised: Frontier,
    state: Vec<u8>,
}

/// Opening rejects snapshots that omit retained tombstones or frontier state.
#[derive(Debug, thiserror::Error)]
pub enum OpenError<E> {
    #[error("loading the visibility snapshot failed: {0}")]
    Store(#[source] E),
    #[error("the visibility snapshot is malformed: {0}")]
    Malformed(#[source] serde_json::Error),
    #[error("visibility snapshot schema {found} is not the {expected} this build restores")]
    UnsupportedSchema { expected: u32, found: u32 },
    #[error("the visibility snapshot's apply state is unrestorable: {0}")]
    State(#[source] SnapshotError),
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyEnvelopeError<E> {
    #[error(transparent)]
    Decode(#[from] VisibilityFeedError),
    #[error("persisting the visibility projection failed: {0}")]
    Store(#[source] E),
}

/// Persists apply state and advertised coverage in one snapshot.
#[derive(Debug)]
pub struct VisibilityProjection<S> {
    store: S,
    state: VisibilityState,
    advertised: Frontier,
}

impl<S: VisibilitySnapshotStore> VisibilityProjection<S> {
    /// Starts empty if `store` has no snapshot.
    ///
    /// # Errors
    /// Returns [`OpenError`] when reading the store, decoding JSON, validating the schema, or restoring
    /// apply state fails.
    pub fn open(store: S) -> Result<Self, OpenError<S::Error>> {
        let (state, advertised) = match store.load_snapshot().map_err(OpenError::Store)? {
            Some(bytes) => {
                let snapshot: ProjectionSnapshot = serde_json::from_slice(&bytes).map_err(OpenError::Malformed)?;
                if snapshot.schema != VISIBILITY_FEED_SCHEMA {
                    return Err(OpenError::UnsupportedSchema {
                        expected: VISIBILITY_FEED_SCHEMA,
                        found: snapshot.schema,
                    });
                }
                let state = VisibilityState::restore(&snapshot.state).map_err(OpenError::State)?;
                (state, snapshot.advertised)
            }
            None => (VisibilityState::new(), Frontier::default()),
        };
        Ok(Self {
            store,
            state,
            advertised,
        })
    }

    fn encode(state: &VisibilityState, advertised: &Frontier) -> Vec<u8> {
        let snapshot = ProjectionSnapshot {
            schema: VISIBILITY_FEED_SCHEMA,
            advertised: advertised.clone(),
            state: state.encode(),
        };
        serde_json::to_vec(&snapshot).expect("a visibility projection snapshot always serializes to JSON")
    }

    /// Persists the batch before committing state and frontier in memory. A no-op batch skips the save.
    ///
    /// # Errors
    /// Returns the store's error when persisting the converged snapshot fails; the projection is left
    /// unchanged.
    pub fn apply(&mut self, ops: &[VisibilityOp]) -> Result<Vec<ApplyEffect>, S::Error> {
        let mut state = self.state.clone();
        let mut advertised = self.advertised.clone();
        let mut effects = Vec::with_capacity(ops.len());
        let mut dirty = false;
        for op in ops {
            let effect = state.apply(op);
            if matches!(effect, ApplyEffect::Applied) {
                dirty = true;
            }
            if advertised
                .high_water(op.order.epoch)
                .is_none_or(|hw| op.order.serial > hw)
            {
                advertised.acknowledge(op.order.epoch, op.order.serial);
                dirty = true;
            }
            effects.push(effect);
        }
        if dirty {
            self.store.save_snapshot(&Self::encode(&state, &advertised))?;
            self.state = state;
            self.advertised = advertised;
        }
        Ok(effects)
    }

    /// Ignores non-visibility envelopes and applies the rest as one batch.
    ///
    /// # Errors
    /// Returns [`ApplyEnvelopeError::Decode`] when an envelope's visibility payload cannot be read, or
    /// [`ApplyEnvelopeError::Store`] when persisting the applied batch fails.
    pub fn apply_envelopes(
        &mut self,
        envelopes: &[OperationEnvelope],
    ) -> Result<Vec<ApplyEffect>, ApplyEnvelopeError<S::Error>> {
        let ops = envelopes
            .iter()
            .filter_map(|envelope| decode_visibility_op(envelope).transpose())
            .collect::<Result<Vec<_>, _>>()?;
        self.apply(&ops).map_err(ApplyEnvelopeError::Store)
    }

    /// Persists compaction without changing advertised coverage.
    ///
    /// # Errors
    /// Returns the store's error when persisting the compacted snapshot fails; the projection is left
    /// unchanged.
    pub fn compact(&mut self, frontier: &Frontier) -> Result<(), S::Error> {
        let mut state = self.state.clone();
        state.compact(frontier);
        self.store.save_snapshot(&Self::encode(&state, &self.advertised))?;
        self.state = state;
        Ok(())
    }

    #[must_use]
    pub fn visibility(&self, artifact: &ArtifactId) -> Visibility {
        self.state.get(artifact)
    }

    /// Advertises coverage after durable application.
    #[must_use]
    pub const fn advertised(&self) -> &Frontier {
        &self.advertised
    }

    #[must_use]
    pub fn retained_artifacts(&self) -> usize {
        self.state.retained_artifacts()
    }
}
