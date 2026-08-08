//! Selecting and planning background cross-data-center blob copies from the placement ledger.
//!
//! Local-filesystem HA needs each data center to hold its own verified copy of every blob a peer
//! serves. This module reads the [`BlobPlacementRecord`] ledger and answers two questions without
//! touching the network or the transfer engine: which digests the local data center still owes a copy
//! (the backlog scan), and, for one such digest, which verified peer to pull from and where the local
//! copy should land (the plan).
//!
//! Both are pure over their inputs. The scan reads the ledger; the plan takes the caller's authority
//! epoch and target backend as parameters, exactly as [`super::reclamation`] and the reclaim executor
//! take their fences. The orchestrator that mints the epoch, holds the job lease, and drives the actual
//! transfer lives above this module.

use std::cmp::Reverse;
use std::ops::Bound::{Included, Unbounded};

use peryx_identity::ArtifactDigest;
use redb::ReadableTable as _;

use super::{
    BLOB_PLACEMENT, BackendId, BackendLocation, BlobPlacementKey, BlobPlacementRecord, BlobPlacementState,
    DataCenterId, MetaError, MetaStore,
};

/// The most placement groups one backlog scan may read, bounding the work a single pass does.
pub const MAX_COPY_BACKLOG_BATCH: usize = 1_000;

/// A verified peer placement the local data center can copy a digest's bytes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedSource {
    /// The remote placement's full key, naming the peer backend, data center, and location.
    pub key: BlobPlacementKey,
    /// The placement generation, so the planner prefers the most recently accepted copy.
    pub generation: u64,
    /// The verified byte size, carried so the transfer knows the length up front.
    pub size: u64,
}

/// One digest the local data center owes a copy: it holds no verified or in-flight local placement, yet
/// a peer serves verified bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyBacklogEntry {
    pub digest: ArtifactDigest,
    /// The verified peer placements, in ledger key order.
    pub sources: Vec<VerifiedSource>,
}

/// One bounded backlog pass in digest order, with a cursor to resume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyBacklogPage {
    pub entries: Vec<CopyBacklogEntry>,
    /// How many distinct digests the pass classified.
    pub scanned: usize,
    /// The digest to resume after, present only when more digests remain past this page.
    pub next_cursor: Option<String>,
}

/// A rejected backlog scan.
#[derive(Debug, thiserror::Error)]
pub enum CopyBacklogError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("limit must be between 1 and {MAX_COPY_BACKLOG_BATCH}")]
    InvalidLimit,
}

/// The copy one backlog entry resolves to under the caller's authority epoch and target backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyPlan {
    /// Pull the digest from `source` into `target`. Boxed so a plan that copies nothing stays small.
    Copy(Box<CrossDcCopy>),
    /// The caller holds the unassigned epoch, which fences all transfer work; do nothing.
    Fenced,
    /// No verified peer offers the digest, so there is nothing to copy from.
    NoSource,
}

/// A planned copy: the verified peer to read from, the local placement to publish, and the fence the
/// transfer must carry into [`MetaStore::apply_blob_placement`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossDcCopy {
    /// The local placement key the copy publishes to.
    pub target: BlobPlacementKey,
    /// The verified peer placement key the bytes come from.
    pub source: BlobPlacementKey,
    pub size: u64,
    /// The authority epoch the transfer stages and verifies under.
    pub fence: u64,
}

/// Choose the verified peer to copy a backlog entry from, then the local placement to publish it as.
///
/// The source is the highest-generation verified peer, ties broken by ledger key order, mirroring how
/// [`plan_blob_fetch`](../../peryx_ha_distributed/blob_placement/fn.plan_blob_fetch.html) prefers the
/// newest copy. An unassigned epoch (`fence == 0`) fences the work closed, and an entry with no source
/// yields nothing to copy.
#[must_use]
pub fn plan_cross_dc_copy(
    entry: &CopyBacklogEntry,
    local_dc: &DataCenterId,
    target_backend: &BackendId,
    target_location: &BackendLocation,
    fence: u64,
) -> CopyPlan {
    if fence == 0 {
        return CopyPlan::Fenced;
    }
    let Some(source) = preferred_source(&entry.sources) else {
        return CopyPlan::NoSource;
    };
    let target = BlobPlacementKey {
        digest: entry.digest.clone(),
        backend: target_backend.clone(),
        data_center: local_dc.clone(),
        location: target_location.clone(),
    };
    CopyPlan::Copy(Box::new(CrossDcCopy {
        target,
        source: source.key.clone(),
        size: source.size,
        fence,
    }))
}

/// The highest-generation source, ties broken toward the earliest ledger key so the choice is stable.
fn preferred_source(sources: &[VerifiedSource]) -> Option<&VerifiedSource> {
    sources
        .iter()
        .enumerate()
        .min_by_key(|(index, source)| (Reverse(source.generation), *index))
        .map(|(_, source)| source)
}

/// Classify one digest's placement records for `local_dc`. A digest joins the backlog only when the
/// local data center holds no verified copy, has no transfer in flight, and has not revoked the
/// artifact, while at least one peer serves verified bytes.
fn backlog_entry(records: &[BlobPlacementRecord], local_dc: &DataCenterId) -> Option<CopyBacklogEntry> {
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
            BlobPlacementState::Verified { .. } | BlobPlacementState::Pending | BlobPlacementState::Revoked
                if is_local =>
            {
                local_settled = true;
            }
            _ => {}
        }
    }
    if local_settled || sources.is_empty() {
        return None;
    }
    Some(CopyBacklogEntry {
        digest: records[0].key.digest.clone(),
        sources,
    })
}

impl MetaStore {
    /// Scan the placement ledger in digest order for the copies `local_dc` owes, resuming after
    /// `cursor` and reading at most `limit` distinct digests.
    ///
    /// The ledger keys its rows by digest first, so every placement of a digest is contiguous and the
    /// pass classifies one digest at a time. It bounds work by digest count rather than by matches: a
    /// full pass carries a `next_cursor` at its last digest, and a pass that reaches the end carries
    /// none.
    ///
    /// # Errors
    /// Returns [`CopyBacklogError::InvalidLimit`] for a limit outside `1..=MAX_COPY_BACKLOG_BATCH`, or a
    /// store error when a row cannot be read or decoded.
    pub fn scan_cross_dc_copy_backlog(
        &self,
        local_dc: &DataCenterId,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<CopyBacklogPage, CopyBacklogError> {
        if !(1..=MAX_COPY_BACKLOG_BATCH).contains(&limit) {
            return Err(CopyBacklogError::InvalidLimit);
        }
        let txn = self.db.begin_read().map_err(MetaError::from)?;
        let table = txn.open_table(BLOB_PLACEMENT).map_err(MetaError::from)?;
        // Resume past every row of the cursor digest: `\u{1}` sorts above the `\0` key separator, so it
        // clears the cursor's placements without landing inside the next digest.
        let resume = cursor.map(|digest| format!("{digest}\u{1}"));
        let entries_iter = resume
            .as_ref()
            .map_or_else(
                || table.iter(),
                |bound| table.range::<&str>((Included(bound.as_str()), Unbounded)),
            )
            .map_err(MetaError::from)?;

        let mut page = CopyBacklogPage {
            entries: Vec::new(),
            scanned: 0,
            next_cursor: None,
        };
        let mut group: Vec<BlobPlacementRecord> = Vec::new();
        for entry in entries_iter {
            let (_key, value) = entry.map_err(MetaError::from)?;
            let record: BlobPlacementRecord = serde_json::from_slice(value.value()).map_err(MetaError::from)?;
            if group.first().is_some_and(|first| first.key.digest != record.key.digest) {
                fold_group(&mut page, &group, local_dc);
                if page.scanned == limit {
                    // The current record opens a further digest, so more work remains past this page.
                    page.next_cursor = Some(group[0].key.digest.canonical());
                    return Ok(page);
                }
                group.clear();
            }
            group.push(record);
        }
        // The final group closes at the end of the table, so it never carries a resume cursor.
        if !group.is_empty() {
            fold_group(&mut page, &group, local_dc);
        }
        Ok(page)
    }
}

/// Fold one completed digest group into the page: record its backlog entry, if any, and count it.
fn fold_group(page: &mut CopyBacklogPage, group: &[BlobPlacementRecord], local_dc: &DataCenterId) {
    if let Some(entry) = backlog_entry(group, local_dc) {
        page.entries.push(entry);
    }
    page.scanned += 1;
}
