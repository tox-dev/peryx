//! Where a content-addressed blob physically resides across backends and data centers.
//!
//! This ledger is distinct from the neutral per-digest [`ArtifactPlacement`](super::ArtifactPlacement)
//! availability projection: it records one row per `(digest, backend, data center, location)` and
//! tracks that location's transfer lifecycle under a fencing epoch, so a routing pass can tell a local
//! copy from a verified remote one, a pending transfer, a failed candidate, or a revoked placement.
//!
//! The transfer engine, remote streaming, and any UI live above this module; here we only persist the
//! evidence-based state and answer routing questions against it.

use std::ops::Bound::{Excluded, Included};

use peryx_identity::ArtifactDigest;
use redb::ReadableTable as _;
use serde::{Deserialize, Serialize};

use super::{BLOB_PLACEMENT, MetaError, MetaStore};

const MAX_COMPONENT_BYTES: usize = 512;

/// The most placements one digest may hold, bounding a digest's routing fan-out and its history scan.
pub const MAX_PLACEMENTS_PER_DIGEST: usize = 64;

/// A rejected placement-key component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PlacementKeyError {
    #[error("{field} must not be empty")]
    Empty { field: &'static str },
    #[error("{field} must be at most {MAX_COMPONENT_BYTES} bytes")]
    TooLong { field: &'static str },
    #[error("{field} must not contain a NUL byte")]
    ContainsNul { field: &'static str },
}

fn component(field: &'static str, value: String) -> Result<String, PlacementKeyError> {
    if value.is_empty() {
        return Err(PlacementKeyError::Empty { field });
    }
    if value.len() > MAX_COMPONENT_BYTES {
        return Err(PlacementKeyError::TooLong { field });
    }
    if value.contains('\0') {
        return Err(PlacementKeyError::ContainsNul { field });
    }
    Ok(value)
}

macro_rules! key_component {
    ($name:ident, $field:literal, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            #[doc = concat!("Validate a ", $field, " component of a placement key.")]
            ///
            /// # Errors
            /// Returns a [`PlacementKeyError`] when the value is empty, over the byte bound, or holds a
            /// NUL byte, which the composite key reserves as its separator.
            pub fn new(value: impl Into<String>) -> Result<Self, PlacementKeyError> {
                Ok(Self(component($field, value.into())?))
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

key_component!(
    BackendId,
    "backend",
    "The blob backend a placement lives on, such as `filesystem` or `s3`."
);
key_component!(
    DataCenterId,
    "data center",
    "The data center a placement lives in, an opaque operator-assigned label."
);
key_component!(
    BackendLocation,
    "location",
    "Where a digest sits inside its backend: an object key or a store-relative path."
);

impl BackendId {
    /// The identity of a built-in backend whose name is a statically known valid component, so a
    /// caller inside the crate names it without a fallible parse.
    pub(crate) fn from_static(name: &'static str) -> Self {
        Self(name.to_owned())
    }
}

impl BackendLocation {
    /// The location a content-addressed blob occupies: its digest, which the backend derives the storage
    /// path from. A digest is always a valid component, so this needs no fallible parse.
    #[must_use]
    pub fn for_digest(digest: &ArtifactDigest) -> Self {
        Self(digest.sha256().to_owned())
    }
}

/// The identity of one blob placement: a digest on a specific backend, data center, and location.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobPlacementKey {
    pub digest: ArtifactDigest,
    pub backend: BackendId,
    pub data_center: DataCenterId,
    pub location: BackendLocation,
}

impl BlobPlacementKey {
    pub(crate) fn encode(&self) -> String {
        format!(
            "{}\0{}\0{}\0{}",
            self.digest.canonical(),
            self.backend.as_str(),
            self.data_center.as_str(),
            self.location.as_str()
        )
    }

    pub(crate) fn digest_bounds(digest: &ArtifactDigest) -> (String, String) {
        let canonical = digest.canonical();
        (format!("{canonical}\0"), format!("{canonical}\u{1}"))
    }
}

/// A classified transfer failure, kept as low-cardinality evidence rather than a raw error string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlobPlacementFailure {
    /// The source backend or peer could not be reached; retry or reselect a source.
    SourceUnavailable,
    /// The transferred bytes hashed to a different digest; the candidate can never serve.
    DigestMismatch,
    /// The backend refused the write with a terminal, non-retryable response.
    BackendRejected,
}

/// The evidence-based lifecycle of one placement.
///
/// A staged temporary file is only [`Pending`](Self::Pending); it becomes [`Verified`](Self::Verified)
/// after the backend confirms the object with a matching digest, which is the sole state routing may
/// serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum BlobPlacementState {
    /// A transfer is in flight or staged; no serving evidence exists yet.
    Pending,
    /// The backend confirmed the object with a matching digest and this byte size.
    Verified { size: u64 },
    /// A transfer attempt failed for a classified reason, or a verified copy failed an integrity re-check
    /// (a [`DigestMismatch`](BlobPlacementFailure::DigestMismatch)) and demoted here. A verified placement
    /// in another location is a different key and is left untouched.
    Failed { class: BlobPlacementFailure },
    /// The placement was withdrawn from serving, for reclamation or an administrative decision.
    Revoked,
}

impl BlobPlacementState {
    #[must_use]
    pub const fn status(&self) -> BlobPlacementStatus {
        match self {
            Self::Pending => BlobPlacementStatus::Pending,
            Self::Verified { .. } => BlobPlacementStatus::Verified,
            Self::Failed { .. } => BlobPlacementStatus::Failed,
            Self::Revoked => BlobPlacementStatus::Revoked,
        }
    }
}

/// The lifecycle category of a placement, without its evidence payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlobPlacementStatus {
    Pending,
    Verified,
    Failed,
    Revoked,
}

/// The current placement record for one key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobPlacementRecord {
    pub key: BlobPlacementKey,
    pub state: BlobPlacementState,
    /// The authority epoch that last wrote this record; a lower epoch is a stale writer and is fenced
    /// out without changing the record.
    pub fence: u64,
    /// The count of accepted transitions, so a reader can detect a concurrent change.
    pub generation: u64,
    pub updated_at_unix: i64,
}

/// One evidence-carrying step a caller applies to a placement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobPlacementTransition {
    /// Begin or retry a transfer into this location, moving it to
    /// [`Pending`](BlobPlacementState::Pending).
    Stage,
    /// Prove completion with the backend's observed digest and byte size.
    Verify { observed: ArtifactDigest, size: u64 },
    /// Record a classified transfer failure from a pending transfer, or demote a verified copy that failed
    /// an integrity re-check via [`DigestMismatch`](BlobPlacementFailure::DigestMismatch).
    Fail { class: BlobPlacementFailure },
    /// Withdraw the placement from serving.
    Revoke,
}

impl BlobPlacementTransition {
    const fn label(&self) -> &'static str {
        match self {
            Self::Stage => "stage",
            Self::Verify { .. } => "verify",
            Self::Fail { .. } => "fail",
            Self::Revoke => "revoke",
        }
    }
}

/// The effect of applying a transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobPlacementOutcome {
    /// The transition changed the record.
    Applied(BlobPlacementRecord),
    /// The transition was a no-op against the current state and left the record unchanged.
    Unchanged(BlobPlacementRecord),
}

impl BlobPlacementOutcome {
    #[must_use]
    pub const fn record(&self) -> &BlobPlacementRecord {
        match self {
            Self::Applied(record) | Self::Unchanged(record) => record,
        }
    }
}

/// A rejected placement transition.
#[derive(Debug, thiserror::Error)]
pub enum BlobPlacementError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("cannot {transition} a placement in the {from:?} state")]
    IllegalTransition {
        from: BlobPlacementStatus,
        transition: &'static str,
    },
    #[error("cannot {transition} a placement that does not exist")]
    MissingPlacement { transition: &'static str },
    #[error("a newer fence {current} supersedes the applied fence {applied}")]
    StaleFence { current: u64, applied: u64 },
    #[error("a digest cannot exceed {MAX_PLACEMENTS_PER_DIGEST} placements")]
    TooManyPlacements,
}

/// Placements for one digest split into the categories a router chooses between.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlobPlacementRouting {
    /// Verified placements in the querying node's own data center.
    pub local: Vec<BlobPlacementRecord>,
    /// Verified placements a remote data center can serve.
    pub verified_remote: Vec<BlobPlacementRecord>,
    pub pending: Vec<BlobPlacementRecord>,
    pub failed: Vec<BlobPlacementRecord>,
    pub revoked: Vec<BlobPlacementRecord>,
}

impl BlobPlacementRouting {
    /// Whether any verified placement, local or remote, can serve the digest.
    #[must_use]
    pub const fn is_serveable(&self) -> bool {
        !self.local.is_empty() || !self.verified_remote.is_empty()
    }

    /// Whether the digest has no placement in any state.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.local.is_empty()
            && self.verified_remote.is_empty()
            && self.pending.is_empty()
            && self.failed.is_empty()
            && self.revoked.is_empty()
    }
}

/// What a transition resolves to before any capacity check, carrying the prior record so the caller
/// never has to reach back for it.
enum Decision {
    /// The record moves to `state`; `prior` is the record it replaces, if any.
    Change {
        state: BlobPlacementState,
        prior: Option<BlobPlacementRecord>,
    },
    /// The transition is a no-op against the current state.
    Unchanged(BlobPlacementRecord),
    /// The transition is not legal from the current state.
    Illegal { from: BlobPlacementStatus },
    /// The transition needs a placement that does not exist.
    Missing,
}

fn decide(
    prior: Option<BlobPlacementRecord>,
    transition: &BlobPlacementTransition,
    digest: &ArtifactDigest,
) -> Decision {
    use BlobPlacementState as S;
    use BlobPlacementTransition as T;
    let change = |state, prior| Decision::Change { state, prior };
    match (prior, transition) {
        (None, T::Stage) => change(S::Pending, None),
        (Some(record), T::Stage) if matches!(record.state, S::Failed { .. }) => change(S::Pending, Some(record)),
        (Some(record), T::Stage) if matches!(record.state, S::Pending) => Decision::Unchanged(record),
        (Some(record), T::Verify { observed, size }) if matches!(record.state, S::Pending) => {
            let state = if observed == digest {
                S::Verified { size: *size }
            } else {
                S::Failed {
                    class: BlobPlacementFailure::DigestMismatch,
                }
            };
            change(state, Some(record))
        }
        (Some(record), T::Fail { class }) if matches!(record.state, S::Pending) => {
            change(S::Failed { class: *class }, Some(record))
        }
        // A verified copy that fails an integrity re-check has bytes that no longer match its address, so
        // it demotes to a digest-mismatch failure - the one failure a settled copy can develop on its own,
        // which makes it a re-copy candidate again rather than a served but corrupt placement.
        (
            Some(record),
            T::Fail {
                class: BlobPlacementFailure::DigestMismatch,
            },
        ) if matches!(record.state, S::Verified { .. }) => change(
            S::Failed {
                class: BlobPlacementFailure::DigestMismatch,
            },
            Some(record),
        ),
        (Some(record), T::Fail { .. }) if matches!(record.state, S::Failed { .. }) => Decision::Unchanged(record),
        (Some(record), T::Revoke) if matches!(record.state, S::Revoked) => Decision::Unchanged(record),
        (Some(record), T::Revoke) => change(S::Revoked, Some(record)),
        (Some(record), _) => Decision::Illegal {
            from: record.state.status(),
        },
        (None, _) => Decision::Missing,
    }
}

impl MetaStore {
    /// Apply one evidence-based transition to a placement under a fencing epoch.
    ///
    /// A `fence` below the stored epoch is a stale writer and is rejected without change. A verify whose
    /// observed digest does not match the key's digest records a
    /// [`DigestMismatch`](BlobPlacementFailure::DigestMismatch) failure rather than a verified
    /// placement.
    ///
    /// # Errors
    /// Returns a [`BlobPlacementError`] for an illegal transition, a stale fence, or a digest that
    /// already holds the maximum number of placements, or a store error when the row cannot be read,
    /// encoded, or committed.
    pub fn apply_blob_placement(
        &self,
        key: &BlobPlacementKey,
        transition: &BlobPlacementTransition,
        fence: u64,
        now: i64,
    ) -> Result<BlobPlacementOutcome, BlobPlacementError> {
        let encoded_key = key.encode();
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        let existing = {
            let table = txn.open_table(BLOB_PLACEMENT).map_err(MetaError::from)?;
            table
                .get(encoded_key.as_str())
                .map_err(MetaError::from)?
                .map(|value| serde_json::from_slice::<BlobPlacementRecord>(value.value()))
                .transpose()
                .map_err(MetaError::from)?
        };
        if let Some(record) = &existing
            && fence < record.fence
        {
            return Err(BlobPlacementError::StaleFence {
                current: record.fence,
                applied: fence,
            });
        }
        let (state, prior) = match decide(existing, transition, &key.digest) {
            Decision::Change { state, prior } => (state, prior),
            Decision::Unchanged(record) => return Ok(BlobPlacementOutcome::Unchanged(record)),
            Decision::Illegal { from } => {
                return Err(BlobPlacementError::IllegalTransition {
                    from,
                    transition: transition.label(),
                });
            }
            Decision::Missing => {
                return Err(BlobPlacementError::MissingPlacement {
                    transition: transition.label(),
                });
            }
        };
        if prior.is_none() {
            guard_capacity(&txn, &key.digest)?;
        }
        let record = BlobPlacementRecord {
            key: key.clone(),
            state,
            fence: prior.as_ref().map_or(fence, |record| record.fence.max(fence)),
            generation: prior.as_ref().map_or(1, |record| record.generation + 1),
            updated_at_unix: now,
        };
        let value = serde_json::to_vec(&record).map_err(MetaError::from)?;
        txn.open_table(BLOB_PLACEMENT)
            .map_err(MetaError::from)?
            .insert(encoded_key.as_str(), value.as_slice())
            .map_err(MetaError::from)?;
        txn.commit().map_err(MetaError::from)?;
        Ok(BlobPlacementOutcome::Applied(record))
    }

    /// Read one placement by its full key.
    ///
    /// # Errors
    /// Returns a store error when the row cannot be read or decoded.
    pub fn blob_placement(&self, key: &BlobPlacementKey) -> Result<Option<BlobPlacementRecord>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(BLOB_PLACEMENT)?;
        Ok(table
            .get(key.encode().as_str())?
            .map(|value| serde_json::from_slice(value.value()))
            .transpose()?)
    }

    /// Record this node's own datacenter as a verified holder of `digest`, once its bytes have committed
    /// and verified at their content address on publish. A peer that replicates the ledger reads this row
    /// as a `verified_remote` source and routes a cross-datacenter read-through fetch to this node, the
    /// producer half of that path: without it a home publish leaves no placement for a sibling to pull.
    ///
    /// The transfer machinery models a copy as [`Stage`](BlobPlacementTransition::Stage) then
    /// [`Verify`](BlobPlacementTransition::Verify); a home publish already holds the verified bytes, so
    /// this drives both under the publish's own `fence` epoch. A digest already verified here is left as
    /// is, so a re-push is a no-op rather than an illegal transition.
    ///
    /// # Errors
    /// Returns a [`BlobPlacementError`] when the ledger cannot be read or written, the fence is stale, or
    /// the digest already holds the maximum number of placements.
    pub fn record_local_placement(
        &self,
        backend: &BackendId,
        data_center: &DataCenterId,
        digest: &ArtifactDigest,
        size: u64,
        fence: u64,
        now: i64,
    ) -> Result<BlobPlacementOutcome, BlobPlacementError> {
        let key = BlobPlacementKey {
            digest: digest.clone(),
            backend: backend.clone(),
            data_center: data_center.clone(),
            location: BackendLocation::for_digest(digest),
        };
        if let Some(record) = self.blob_placement(&key)?
            && matches!(record.state, BlobPlacementState::Verified { .. })
        {
            return Ok(BlobPlacementOutcome::Unchanged(record));
        }
        self.apply_blob_placement(&key, &BlobPlacementTransition::Stage, fence, now)?;
        self.apply_blob_placement(
            &key,
            &BlobPlacementTransition::Verify {
                observed: digest.clone(),
                size,
            },
            fence,
            now,
        )
    }

    /// List every placement for one digest in canonical key order.
    ///
    /// # Errors
    /// Returns a store error when a row cannot be read or decoded.
    pub fn blob_placements(&self, digest: &ArtifactDigest) -> Result<Vec<BlobPlacementRecord>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(BLOB_PLACEMENT)?;
        let (low, high) = BlobPlacementKey::digest_bounds(digest);
        let mut records = Vec::new();
        for entry in table.range::<&str>((Included(low.as_str()), Excluded(high.as_str())))? {
            let (_key, value) = entry?;
            records.push(serde_json::from_slice(value.value())?);
        }
        Ok(records)
    }

    /// Classify a digest's placements for a router in `local_dc`.
    ///
    /// # Errors
    /// Returns a store error when a row cannot be read or decoded.
    pub fn route_blob_placements(
        &self,
        digest: &ArtifactDigest,
        local_dc: &DataCenterId,
    ) -> Result<BlobPlacementRouting, MetaError> {
        let mut routing = BlobPlacementRouting::default();
        for record in self.blob_placements(digest)? {
            match record.state.status() {
                BlobPlacementStatus::Verified if &record.key.data_center == local_dc => routing.local.push(record),
                BlobPlacementStatus::Verified => routing.verified_remote.push(record),
                BlobPlacementStatus::Pending => routing.pending.push(record),
                BlobPlacementStatus::Failed => routing.failed.push(record),
                BlobPlacementStatus::Revoked => routing.revoked.push(record),
            }
        }
        Ok(routing)
    }
}

fn guard_capacity(txn: &redb::WriteTransaction, digest: &ArtifactDigest) -> Result<(), BlobPlacementError> {
    let (low, high) = BlobPlacementKey::digest_bounds(digest);
    let table = txn.open_table(BLOB_PLACEMENT).map_err(MetaError::from)?;
    let count = table
        .range::<&str>((Included(low.as_str()), Excluded(high.as_str())))
        .map_err(MetaError::from)?
        .count();
    if count >= MAX_PLACEMENTS_PER_DIGEST {
        return Err(BlobPlacementError::TooManyPlacements);
    }
    Ok(())
}
