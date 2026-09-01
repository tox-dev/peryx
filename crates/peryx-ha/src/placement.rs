use peryx_identity::ArtifactDigest;
use serde::{Deserialize, Serialize};

const MAX_COMPONENT_BYTES: usize = 512;

pub const MAX_PLACEMENTS_PER_DIGEST: usize = 64;
pub const MAX_REPAIR_BATCH: usize = 1_000;

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
    ($name:ident, $field:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// # Errors
            /// Returns an error for an empty value, a value over 512 bytes, or a NUL byte.
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

key_component!(BackendId, "backend");
key_component!(DataCenterId, "data center");
key_component!(BackendLocation, "location");

impl BackendId {
    #[must_use]
    pub fn from_static(name: &'static str) -> Self {
        Self(name.to_owned())
    }
}

impl BackendLocation {
    #[must_use]
    pub fn for_digest(digest: &ArtifactDigest) -> Self {
        Self(digest.sha256().to_owned())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactSource {
    Hosted,
    Proxy,
    Generated,
}

impl ArtifactSource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hosted => "hosted",
            Self::Proxy => "proxy",
            Self::Generated => "generated",
        }
    }

    #[must_use]
    pub const fn has_upstream(self) -> bool {
        matches!(self, Self::Proxy)
    }

    const fn without_bytes(self) -> ByteAvailability {
        if self.has_upstream() {
            ByteAvailability::RemoteOnly
        } else {
            ByteAvailability::Unavailable
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ByteAvailability {
    Local,
    RemoteOnly,
    Unavailable,
}

impl ByteAvailability {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::RemoteOnly => "remote_only",
            Self::Unavailable => "unavailable",
        }
    }

    #[must_use]
    pub const fn is_local(self) -> bool {
        matches!(self, Self::Local)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementEvent {
    BytesVerified,
    WriteFailed,
    BytesRemoved,
    Repaired { present: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactPlacement {
    pub source: ArtifactSource,
    pub availability: ByteAvailability,
}

impl ArtifactPlacement {
    #[must_use]
    pub const fn record(source: ArtifactSource, present: bool) -> Self {
        Self {
            source,
            availability: Self::from_presence(source, present),
        }
    }

    #[must_use]
    pub const fn after(self, event: PlacementEvent) -> Self {
        let availability = match event {
            PlacementEvent::BytesVerified => ByteAvailability::Local,
            PlacementEvent::BytesRemoved => self.source.without_bytes(),
            PlacementEvent::WriteFailed => self.availability,
            PlacementEvent::Repaired { present } => Self::from_presence(self.source, present),
        };
        Self { availability, ..self }
    }

    const fn from_presence(source: ArtifactSource, present: bool) -> ByteAvailability {
        if present {
            ByteAvailability::Local
        } else {
            source.without_bytes()
        }
    }
}

pub trait ArtifactOrigin {
    fn artifact_source(&self) -> ArtifactSource;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArtifactPlacementRow {
    pub digest: String,
    pub source: ArtifactSource,
    pub availability: ByteAvailability,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactPlacementQuery {
    pub cursor: Option<String>,
    pub limit: usize,
}

impl Default for ArtifactPlacementQuery {
    fn default() -> Self {
        Self {
            cursor: None,
            limit: 25,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArtifactPlacementPage {
    pub rows: Vec<ArtifactPlacementRow>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct ArtifactPlacementHealth {
    pub local: u64,
    pub remote_only: u64,
    pub unavailable: u64,
}

impl ArtifactPlacementHealth {
    #[must_use]
    pub const fn total(self) -> u64 {
        self.local + self.remote_only + self.unavailable
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementRepairPage {
    pub scanned: usize,
    pub reconciled: usize,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobPlacementKey {
    pub digest: ArtifactDigest,
    pub backend: BackendId,
    pub data_center: DataCenterId,
    pub location: BackendLocation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlobPlacementFailure {
    SourceUnavailable,
    DigestMismatch,
    BackendRejected,
    /// A byte cap this node applies to its own transfers, so retrying the same source repeats it.
    TransferLimit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum BlobPlacementState {
    Pending,
    Verified { size: u64 },
    Failed { class: BlobPlacementFailure },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlobPlacementStatus {
    Pending,
    Verified,
    Failed,
    Revoked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobPlacementRecord {
    pub key: BlobPlacementKey,
    pub state: BlobPlacementState,
    pub fence: u64,
    #[serde(default)]
    pub transfer_attempt: u64,
    pub generation: u64,
    pub updated_at_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobPlacementTransition {
    Stage,
    Checkpoint {
        attempt: u64,
    },
    Verify {
        attempt: u64,
        observed: ArtifactDigest,
        size: u64,
    },
    Fail {
        attempt: u64,
        class: BlobPlacementFailure,
    },
    Invalidate,
    Revoke,
}

impl BlobPlacementTransition {
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Stage => "stage",
            Self::Checkpoint { .. } => "checkpoint",
            Self::Verify { .. } => "verify",
            Self::Fail { .. } => "fail",
            Self::Invalidate => "invalidate",
            Self::Revoke => "revoke",
        }
    }

    const fn attempt(&self) -> Option<u64> {
        match self {
            Self::Checkpoint { attempt } | Self::Verify { attempt, .. } | Self::Fail { attempt, .. } => Some(*attempt),
            Self::Stage | Self::Invalidate | Self::Revoke => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobPlacementOutcome {
    Applied(BlobPlacementRecord),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BlobPlacementDecisionError {
    #[error("cannot {transition} a placement in the {from:?} state")]
    IllegalTransition {
        from: BlobPlacementStatus,
        transition: &'static str,
    },
    #[error("cannot {transition} a placement that does not exist")]
    MissingPlacement { transition: &'static str },
    #[error("a newer fence {current} supersedes the applied fence {applied}")]
    StaleFence { current: u64, applied: u64 },
    #[error("transfer attempt {current} supersedes attempt {applied}")]
    StaleTransferAttempt { current: u64, applied: u64 },
    #[error("transfer attempt fence {current} does not match fence {applied}")]
    TransferAttemptFenceMismatch { current: u64, applied: u64 },
    #[error("the transfer attempt counter is exhausted")]
    TransferAttemptExhausted,
}

/// # Errors
///
/// Returns an error for stale ownership, a missing placement, an exhausted attempt counter, or an
/// illegal transition.
pub fn decide_blob_placement(
    key: &BlobPlacementKey,
    prior: Option<&BlobPlacementRecord>,
    transition: &BlobPlacementTransition,
    fence: u64,
    now: i64,
) -> Result<BlobPlacementOutcome, BlobPlacementDecisionError> {
    if let Some(record) = prior
        && fence < record.fence
    {
        return Err(BlobPlacementDecisionError::StaleFence {
            current: record.fence,
            applied: fence,
        });
    }
    if let (Some(record), Some(attempt)) = (prior, transition.attempt()) {
        if fence != record.fence {
            return Err(BlobPlacementDecisionError::TransferAttemptFenceMismatch {
                current: record.fence,
                applied: fence,
            });
        }
        if attempt != record.transfer_attempt {
            return Err(BlobPlacementDecisionError::StaleTransferAttempt {
                current: record.transfer_attempt,
                applied: attempt,
            });
        }
    }
    match next_blob_placement(key, prior, transition, fence)? {
        NextBlobPlacement::Unchanged(record) => Ok(BlobPlacementOutcome::Unchanged(record.clone())),
        NextBlobPlacement::Applied(state) => Ok(BlobPlacementOutcome::Applied(BlobPlacementRecord {
            key: key.clone(),
            state,
            fence: prior.map_or(fence, |record| record.fence.max(fence)),
            transfer_attempt: if matches!(transition, BlobPlacementTransition::Stage) {
                prior.map_or(Ok(1), |record| {
                    record
                        .transfer_attempt
                        .checked_add(1)
                        .ok_or(BlobPlacementDecisionError::TransferAttemptExhausted)
                })?
            } else {
                prior.map_or(0, |record| record.transfer_attempt)
            },
            generation: prior.map_or(1, |record| record.generation + 1),
            updated_at_unix: now,
        })),
    }
}

enum NextBlobPlacement<'a> {
    Unchanged(&'a BlobPlacementRecord),
    Applied(BlobPlacementState),
}

fn next_blob_placement<'a>(
    key: &BlobPlacementKey,
    prior: Option<&'a BlobPlacementRecord>,
    transition: &BlobPlacementTransition,
    fence: u64,
) -> Result<NextBlobPlacement<'a>, BlobPlacementDecisionError> {
    use BlobPlacementState as State;
    use BlobPlacementTransition as Transition;
    use NextBlobPlacement::{Applied, Unchanged};

    match (prior, transition) {
        (None, Transition::Stage) => Ok(Applied(State::Pending)),
        (Some(record), Transition::Stage) if matches!(record.state, State::Failed { .. } | State::Revoked) => {
            Ok(Applied(State::Pending))
        }
        (Some(record), Transition::Stage) if matches!(record.state, State::Pending) && fence > record.fence => {
            Ok(Applied(State::Pending))
        }
        (Some(record), Transition::Stage) if matches!(record.state, State::Pending) => Ok(Unchanged(record)),
        (Some(record), Transition::Checkpoint { .. }) if matches!(record.state, State::Pending) => {
            Ok(Applied(State::Pending))
        }
        (Some(record), Transition::Verify { observed, size, .. }) if matches!(record.state, State::Pending) => {
            Ok(Applied(if observed == &key.digest {
                State::Verified { size: *size }
            } else {
                State::Failed {
                    class: BlobPlacementFailure::DigestMismatch,
                }
            }))
        }
        (Some(record), Transition::Fail { class, .. }) if matches!(record.state, State::Pending) => {
            Ok(Applied(State::Failed { class: *class }))
        }
        (Some(record), Transition::Invalidate) if matches!(record.state, State::Verified { .. }) => {
            Ok(Applied(State::Failed {
                class: BlobPlacementFailure::DigestMismatch,
            }))
        }
        (Some(record), Transition::Fail { .. }) if matches!(record.state, State::Failed { .. }) => {
            Ok(Unchanged(record))
        }
        (Some(record), Transition::Revoke) if matches!(record.state, State::Revoked) => Ok(Unchanged(record)),
        (Some(_), Transition::Revoke) => Ok(Applied(State::Revoked)),
        (Some(record), _) => Err(BlobPlacementDecisionError::IllegalTransition {
            from: record.state.status(),
            transition: transition.label(),
        }),
        (None, _) => Err(BlobPlacementDecisionError::MissingPlacement {
            transition: transition.label(),
        }),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlobPlacementRouting {
    pub local: Vec<BlobPlacementRecord>,
    pub verified_remote: Vec<BlobPlacementRecord>,
    pub pending: Vec<BlobPlacementRecord>,
    pub failed: Vec<BlobPlacementRecord>,
    pub revoked: Vec<BlobPlacementRecord>,
}

impl BlobPlacementRouting {
    #[must_use]
    pub const fn is_serveable(&self) -> bool {
        !self.local.is_empty() || !self.verified_remote.is_empty()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.local.is_empty()
            && self.verified_remote.is_empty()
            && self.pending.is_empty()
            && self.failed.is_empty()
            && self.revoked.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlobPlacementPage {
    pub records: Vec<BlobPlacementRecord>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlobPlacementGroupPage {
    pub groups: Vec<Vec<BlobPlacementRecord>>,
    pub next_cursor: Option<String>,
}
