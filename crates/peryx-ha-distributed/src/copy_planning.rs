use std::cmp::Reverse;
use std::num::NonZeroU64;

use peryx_ha::{BackendId, BackendLocation, BlobPlacementKey, BlobPlacementRecord, BlobPlacementState, DataCenterId};
use peryx_identity::ArtifactDigest;

#[derive(Debug, Clone, PartialEq, Eq)]
struct VerifiedSource {
    key: BlobPlacementKey,
    generation: u64,
    size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyBacklogEntry {
    digest: ArtifactDigest,
    source: VerifiedSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossDcCopy {
    pub target: BlobPlacementKey,
    pub source: BlobPlacementKey,
    pub size: u64,
    pub fence: NonZeroU64,
}

pub fn copy_backlog_entry(
    records: &[BlobPlacementRecord],
    local_dc: &DataCenterId,
    fence: NonZeroU64,
) -> Option<CopyBacklogEntry> {
    let mut local_settled = false;
    let mut sources = Vec::new();
    for record in records {
        let is_local = &record.key.data_center == local_dc;
        match record.state {
            BlobPlacementState::Verified { size } if !is_local => sources.push(VerifiedSource {
                key: record.key.clone(),
                generation: record.generation,
                size,
            }),
            BlobPlacementState::Verified { .. } | BlobPlacementState::Revoked if is_local => {
                local_settled = true;
            }
            BlobPlacementState::Pending if is_local && record.fence >= fence.get() => local_settled = true,
            _ => {}
        }
    }
    if local_settled {
        return None;
    }
    Some(CopyBacklogEntry {
        digest: records.first()?.key.digest.clone(),
        source: sources
            .into_iter()
            .enumerate()
            .min_by_key(|(index, source)| (Reverse(source.generation), *index))?
            .1,
    })
}

pub fn plan_cross_dc_copy(
    entry: &CopyBacklogEntry,
    local_dc: &DataCenterId,
    target_backend: &BackendId,
    fence: NonZeroU64,
) -> CrossDcCopy {
    CrossDcCopy {
        target: BlobPlacementKey {
            digest: entry.digest.clone(),
            backend: target_backend.clone(),
            data_center: local_dc.clone(),
            location: BackendLocation::for_digest(&entry.digest),
        },
        source: entry.source.key.clone(),
        size: entry.source.size,
        fence,
    }
}

#[cfg(test)]
#[path = "../tests/unit/copy_planning_tests.rs"]
mod tests;
