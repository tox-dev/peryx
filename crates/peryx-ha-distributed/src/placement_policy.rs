use peryx_ha::{
    ArtifactPlacement, ArtifactSource, BackendId, BackendLocation, BlobPlacementDecisionError, BlobPlacementKey,
    BlobPlacementOutcome, BlobPlacementRouting, BlobPlacementState, BlobPlacementStatus, BlobPlacementTransition,
    CompareWrite, DataCenterId, PlacementEvent, decide_blob_placement,
};
use peryx_identity::ArtifactDigest;
use peryx_storage::meta::{MetaError, MetaStore};

pub struct DistributedHomePlacementRecorder {
    meta: MetaStore,
    backend: BackendId,
    data_center: DataCenterId,
    clock: peryx_core::Clock,
}

impl DistributedHomePlacementRecorder {
    pub fn new(meta: MetaStore, backend: BackendId, data_center: DataCenterId, clock: peryx_core::Clock) -> Self {
        Self {
            meta,
            backend,
            data_center,
            clock,
        }
    }
}

impl peryx_ha::HomePlacementRecorder for DistributedHomePlacementRecorder {
    fn record(&self, digest: &str, size: u64, fence: u64) -> Result<(), String> {
        let digest = ArtifactDigest::from_sha256(digest).map_err(|error| error.to_string())?;
        record_local_placement(
            &self.meta,
            &self.backend,
            &self.data_center,
            &digest,
            size,
            fence,
            (self.clock)(),
        )
        .map(|_| ())
        .map_err(|error| error.to_string())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BlobPlacementError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error(transparent)]
    Decision(#[from] BlobPlacementDecisionError),
    #[error("a digest cannot exceed {} placements", peryx_ha::MAX_PLACEMENTS_PER_DIGEST)]
    TooManyPlacements,
}

/// # Errors
/// Returns a decision error or a persistence error.
pub fn apply_blob_placement(
    meta: &MetaStore,
    key: &BlobPlacementKey,
    transition: &BlobPlacementTransition,
    fence: u64,
    now: i64,
) -> Result<BlobPlacementOutcome, BlobPlacementError> {
    loop {
        let prior = meta.blob_placement(key)?;
        let outcome = decide_blob_placement(key, prior.as_ref(), transition, fence, now)?;
        let BlobPlacementOutcome::Applied(replacement) = &outcome else {
            return Ok(outcome);
        };
        match meta.compare_and_put_blob_placement(prior.as_ref(), replacement)? {
            CompareWrite::Written => return Ok(outcome),
            CompareWrite::Conflict => {}
            CompareWrite::CapacityExceeded => return Err(BlobPlacementError::TooManyPlacements),
        }
    }
}

/// # Errors
/// Returns a decision error or a persistence error.
pub fn record_local_placement(
    meta: &MetaStore,
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
    if let Some(record) = meta.blob_placement(&key)?
        && matches!(record.state, BlobPlacementState::Verified { .. })
    {
        return Ok(BlobPlacementOutcome::Unchanged(record));
    }
    apply_blob_placement(meta, &key, &BlobPlacementTransition::Stage, fence, now)?;
    apply_blob_placement(
        meta,
        &key,
        &BlobPlacementTransition::Verify {
            observed: digest.clone(),
            size,
        },
        fence,
        now,
    )
}

#[must_use]
pub fn route_blob_placements(
    records: Vec<peryx_ha::BlobPlacementRecord>,
    local_dc: &DataCenterId,
) -> BlobPlacementRouting {
    let mut routing = BlobPlacementRouting::default();
    for record in records {
        match record.state.status() {
            BlobPlacementStatus::Verified if &record.key.data_center == local_dc => routing.local.push(record),
            BlobPlacementStatus::Verified => routing.verified_remote.push(record),
            BlobPlacementStatus::Pending => routing.pending.push(record),
            BlobPlacementStatus::Failed => routing.failed.push(record),
            BlobPlacementStatus::Revoked => routing.revoked.push(record),
        }
    }
    routing
}

/// # Errors
/// Returns a store error when the placement cannot be written.
pub fn record_artifact_placement(
    meta: &MetaStore,
    digest: &str,
    source: ArtifactSource,
    present: bool,
) -> Result<ArtifactPlacement, MetaError> {
    let placement = ArtifactPlacement::record(source, present);
    meta.put_artifact_placement(digest, &placement)?;
    Ok(placement)
}

/// # Errors
/// Returns a store error when the placement cannot be read or written.
pub fn apply_placement_event(
    meta: &MetaStore,
    digest: &str,
    event: PlacementEvent,
) -> Result<Option<ArtifactPlacement>, MetaError> {
    loop {
        let Some(current) = meta.get_artifact_placement(digest)? else {
            return Ok(None);
        };
        let replacement = current.after(event);
        if replacement == current || meta.compare_and_put_artifact_placement(digest, &current, &replacement)? {
            return Ok(Some(replacement));
        }
    }
}
