//! Threading typed visibility operations through the replication change feed and applying them on a
//! follower before it advertises the operation frontier.
//!
//! A minted [`VisibilityOp`](crate::VisibilityOp) rides the same primary/replica change stream every
//! other mutation travels, not a side channel: it becomes a [`Change`] whose serial is the operation's
//! own journal serial and whose event bytes carry the typed transition, wrapped in an
//! [`OperationEnvelope`] tagged [`OperationKind::Visibility`]. A follower routes a visibility envelope
//! to its [`VisibilityProjection`], which folds the operation into a
//! [`VisibilityState`](crate::VisibilityState), persists the converged apply state, and only then lets
//! the operation count toward the frontier it advertises. Persisting the served projection before
//! advancing the advertised frontier is the ordering the availability contract rests on: a replica
//! never advertises coverage of an operation whose effect it has not durably applied, so a reader that
//! trusts the frontier can trust the projection behind it.
//!
//! The apply is idempotent and monotonic through [`VisibilityState`], so duplicate and reordered
//! delivery cannot resurrect an older visibility, and a batch commits its whole effect or none of it:
//! a persistence failure leaves the projection and its advertised frontier exactly where they were, so
//! the follower retries from a consistent point rather than advertising a state it failed to save.

use serde::{Deserialize, Serialize};

use crate::envelope::{AuthorityEpoch, OperationEnvelope, OperationKind};
use crate::protocol::Change;
use crate::visibility::{
    ApplyEffect, ArtifactId, Frontier, OpOrder, SnapshotError, Visibility, VisibilityAction, VisibilityOp,
    VisibilityState,
};

/// The wire schema this build writes into a visibility change's event bytes and reads back.
///
/// A format change is a deliberate migration, never a silent misread that could apply a transition
/// under the wrong identity.
pub const VISIBILITY_CHANGE_SCHEMA: u32 = 1;

/// The schema of the combined durable snapshot a [`VisibilityProjection`] persists: the retained
/// tombstones and the advertised frontier together, so a restart resumes both.
const VISIBILITY_FEED_SCHEMA: u32 = 1;

/// The serde shape a visibility operation takes inside a [`Change`]'s event bytes.
#[derive(Serialize, Deserialize)]
struct EncodedOp {
    schema: u32,
    artifact: ArtifactId,
    action: VisibilityAction,
    order: OpOrder,
}

/// Encode `op` into the event payload a visibility [`Change`] carries.
fn encode_op(op: &VisibilityOp) -> Vec<u8> {
    let encoded = EncodedOp {
        schema: VISIBILITY_CHANGE_SCHEMA,
        artifact: op.artifact.clone(),
        action: op.action,
        order: op.order,
    };
    serde_json::to_vec(&encoded).expect("a visibility operation always serializes to JSON")
}

/// Build the [`Change`] that carries `op` on the replication feed.
///
/// The change's serial is the operation's own serial, so the operation orders against every other
/// mutation by the one journal serial the availability contract already expresses staleness over.
#[must_use]
pub fn visibility_change(op: &VisibilityOp) -> Change {
    Change {
        serial: op.order.serial,
        event: encode_op(op),
        metadata: Vec::new(),
        blobs: Vec::new(),
    }
}

/// Wrap `op` in the [`OperationEnvelope`] a primary ships to its followers.
///
/// The envelope is [`OperationKind::Visibility`] under the operation's authority epoch and carries the
/// typed transition in its change, so it rides the existing replication transport rather than a second
/// stream.
#[must_use]
pub fn visibility_envelope(source: impl Into<String>, op: &VisibilityOp) -> OperationEnvelope {
    OperationEnvelope::current(
        source,
        AuthorityEpoch(op.order.epoch),
        OperationKind::Visibility,
        visibility_change(op),
    )
}

/// A visibility change that could not be read back into a typed operation.
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

/// Decode the visibility operation a `Visibility` envelope carries, or `None` for an envelope of any
/// other kind so a caller can hand every envelope through and act only on the visibility ones.
///
/// A decoded operation's identity must match its envelope: the operation's serial is the change serial
/// and its epoch is the envelope epoch, the same `(epoch, serial)` the envelope orders by. A mismatch
/// is rejected rather than applied under an identity the transport did not order it by.
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

/// The durable boundary a [`VisibilityProjection`] persists its converged apply state to.
///
/// Production backs it with the metadata store's visibility-snapshot singleton; a follower recovers
/// every tombstone across a restart or a metadata log compaction from the last saved snapshot.
pub trait VisibilitySnapshotStore {
    /// The failure a snapshot read or write can report.
    type Error;

    /// Read the last saved snapshot, or `None` before the first save.
    ///
    /// # Errors
    /// Returns [`Self::Error`] when the underlying store cannot read the snapshot.
    fn load_snapshot(&self) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Overwrite the saved snapshot with `bytes`, so the durable copy reflects the latest apply state.
    ///
    /// # Errors
    /// Returns [`Self::Error`] when the underlying store cannot write the snapshot.
    fn save_snapshot(&self, bytes: &[u8]) -> Result<(), Self::Error>;
}

impl<T: VisibilitySnapshotStore + ?Sized> VisibilitySnapshotStore for &T {
    type Error = T::Error;

    fn load_snapshot(&self) -> Result<Option<Vec<u8>>, Self::Error> {
        (**self).load_snapshot()
    }

    fn save_snapshot(&self, bytes: &[u8]) -> Result<(), Self::Error> {
        (**self).save_snapshot(bytes)
    }
}

impl VisibilitySnapshotStore for peryx_storage::meta::MetaStore {
    type Error = peryx_storage::meta::MetaError;

    fn load_snapshot(&self) -> Result<Option<Vec<u8>>, Self::Error> {
        self.visibility_snapshot()
    }

    fn save_snapshot(&self, bytes: &[u8]) -> Result<(), Self::Error> {
        self.save_visibility_snapshot(bytes)
    }
}

/// The combined durable form of a projection: the schema tag, the advertised frontier, and the
/// retained apply state's own opaque snapshot bytes.
#[derive(Serialize, Deserialize)]
struct ProjectionSnapshot {
    schema: u32,
    advertised: Frontier,
    state: Vec<u8>,
}

/// A visibility snapshot that cannot be trusted to reproduce the follower's projection, so
/// [`open`](VisibilityProjection::open) refuses it rather than resume a projection that silently drops
/// a tombstone.
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

/// A decode or persistence failure while applying a visibility envelope.
#[derive(Debug, thiserror::Error)]
pub enum ApplyEnvelopeError<E> {
    #[error(transparent)]
    Decode(#[from] VisibilityFeedError),
    #[error("persisting the visibility projection failed: {0}")]
    Store(#[source] E),
}

/// A follower's served visibility projection: the replicated apply state, its advertised operation
/// frontier, and the durable store that carries both across a restart.
///
/// [`apply`](Self::apply) folds a batch of operations into the state, persists the converged snapshot,
/// and only then advances the advertised frontier. The whole batch commits or none of it: if the save
/// fails the projection and its frontier stay exactly where they were, so a follower never advertises
/// coverage of an operation it did not durably apply.
#[derive(Debug)]
pub struct VisibilityProjection<S> {
    store: S,
    state: VisibilityState,
    advertised: Frontier,
}

impl<S: VisibilitySnapshotStore> VisibilityProjection<S> {
    /// Open a projection over `store`, restoring the last saved snapshot or starting empty when none
    /// has been saved.
    ///
    /// # Errors
    /// Returns [`OpenError`] when the store read fails, or the saved snapshot is malformed, tagged with
    /// a schema this build does not restore, or carries an unrestorable apply state.
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

    /// Apply `ops` in order, persist the converged projection, and advance the advertised frontier to
    /// cover each operation's serial.
    ///
    /// Each operation folds into the apply state idempotently and monotonically, so a duplicate or a
    /// reordered older delivery leaves the visibility it targets unchanged. The batch persists once and
    /// atomically: a save failure leaves the projection and its advertised frontier untouched, so the
    /// follower retries from the state it last durably held. A batch that changes nothing - every
    /// operation a duplicate that neither moves the state nor raises the advertised frontier - skips
    /// the save.
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

    /// Route `envelopes` to the projection, applying the visibility operations among them and ignoring
    /// every envelope of another kind, then persisting the batch as [`apply`](Self::apply) does.
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

    /// Release every artifact that has returned to the visible default and whose operations `frontier`
    /// covers, persisting the compacted state so the retention bound survives a restart.
    ///
    /// The advertised frontier is untouched: compaction forgets entries the frontier has settled, not
    /// operations the replica has served.
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

    /// The current visibility of `artifact`, the projection a served protocol view reads.
    #[must_use]
    pub fn visibility(&self, artifact: &ArtifactId) -> Visibility {
        self.state.get(artifact)
    }

    /// The operation frontier this replica advertises: the highest serial per epoch whose applied
    /// effect it has durably persisted.
    #[must_use]
    pub const fn advertised(&self) -> &Frontier {
        &self.advertised
    }

    /// The number of artifacts the projection currently retains.
    #[must_use]
    pub fn retained_artifacts(&self) -> usize {
        self.state.retained_artifacts()
    }
}
