//! Commits verified blobs before recording their local placement. Frontier advancement remains separate,
//! so metadata stays unreadable until its blobs are locally present or available through read-through.

use std::collections::{BTreeSet, HashMap};
use std::num::NonZeroUsize;

use bytes::Bytes;
use peryx_ha::{ArtifactSource, PlacementEvent};
use peryx_identity::ArtifactDigest;
use peryx_storage::blob::{BlobStorage, Digest};
use peryx_storage::meta::MetaStore;

use crate::blob::BlobTransport;
use crate::blob_availability::{BlobAvailability, ReferencedBlob, blob_availability};
use crate::blob_fetch::{FetchOutcome, FetchReport, fetch_missing};
use crate::blob_placement::{FetchPlan, plan_blob_fetch};
use crate::blob_pull::{PullError, pull_ranged_blob};
use crate::error::SyncError;
use crate::protocol::{PlacementAvailability, PlacementDescriptor};
use crate::{apply_placement_event, record_artifact_placement};

/// The readable frontier gates metadata visibility on this blob-availability view.
pub const BLOB_VIEW: &str = peryx_ha::AVAILABILITY_BLOB_VIEW;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobPlaneReport {
    /// Blobs fetched, committed, and marked local.
    pub fetched: usize,
    /// Blobs left for a later pass after retryable losses.
    pub pending: usize,
}

/// Repairs placement records after an interrupted commit and fetches missing blobs.
///
/// Retryable losses remain [pending](BlobPlaneReport::pending); terminal losses fail the pass.
///
/// # Errors
/// [`SyncError::BlobFetchFailed`] on a terminal fetch loss, [`SyncError::BlobSizeMismatch`] when a
/// fetched blob is not its referenced size, or a store error committing bytes or recording placement.
pub async fn pull_referenced<T: BlobTransport>(
    transport: &T,
    blobs: &BlobStorage,
    meta: &MetaStore,
    referenced: &[(Digest, u64)],
    concurrency: NonZeroUsize,
) -> Result<BlobPlaneReport, SyncError> {
    let mut absent = Vec::new();
    for (digest, size) in referenced {
        if blobs.head(digest).await?.is_none() {
            absent.push((digest.clone(), *size));
        } else {
            repair_local_placement(meta, digest)?;
        }
    }
    if absent.is_empty() {
        return Ok(BlobPlaneReport { fetched: 0, pending: 0 });
    }
    let digests: Vec<Digest> = absent.iter().map(|(digest, _)| digest.clone()).collect();
    let FetchReport { fetched, outcome } = fetch_missing(transport, &digests, concurrency).await;
    let fetched_count = fetched.len();
    let mut bytes_by_digest: HashMap<Digest, Vec<u8>> = fetched.into_iter().collect();
    // Keep each fetched blob paired with its trusted reference size; retryable losses leave no bytes.
    for (digest, size) in &absent {
        if let Some(bytes) = bytes_by_digest.remove(digest) {
            commit_blob(blobs, meta, digest, *size, bytes).await?;
        }
    }
    match outcome {
        FetchOutcome::Complete => Ok(BlobPlaneReport {
            fetched: fetched_count,
            pending: 0,
        }),
        FetchOutcome::Backpressured { pending } => Ok(BlobPlaneReport {
            fetched: fetched_count,
            pending,
        }),
        FetchOutcome::Failed { reason, digest } => Err(SyncError::BlobFetchFailed {
            reason,
            digest: digest.as_str().to_owned(),
        }),
    }
}

/// `delegates` provides ranged transports and read-through peers by datacenter. `simple` handles other
/// blobs. Empty `delegates` and `local_dc` force the whole-blob path.
pub struct BlobSources<'a, T> {
    pub simple: &'a T,
    pub delegates: &'a HashMap<String, T>,
    pub local_dc: &'a str,
}

enum BlobDisposition {
    Defer,
    Ranged(Vec<String>),
    Whole,
}

/// Defers remote-only blobs to read-through, uses ranges for a local blob with at least two reachable peer
/// copies, and sends all other cases through the whole-blob transport.
fn classify_blob(descriptors: &[PlacementDescriptor], local_dc: &str, reachable: &BTreeSet<String>) -> BlobDisposition {
    let FetchPlan::Sources(ordered) = plan_blob_fetch(descriptors, local_dc) else {
        return BlobDisposition::Whole;
    };
    let has_local = ordered.iter().any(|descriptor| descriptor.data_center == local_dc);
    let mut sources = Vec::new();
    for descriptor in ordered {
        if reachable.contains(&descriptor.data_center) && !sources.contains(&descriptor.data_center) {
            sources.push(descriptor.data_center);
        }
    }
    if !has_local && !sources.is_empty() {
        BlobDisposition::Defer
    } else if sources.len() >= 2 {
        BlobDisposition::Ranged(sources)
    } else {
        BlobDisposition::Whole
    }
}

fn deferred_to_peer(descriptors: &[PlacementDescriptor], local_dc: &str, reachable: &BTreeSet<String>) -> bool {
    matches!(classify_blob(descriptors, local_dc, reachable), BlobDisposition::Defer)
}

/// Journal digests are canonical lowercase SHA-256, as required by [`ArtifactDigest::from_sha256`].
fn placement_descriptors(meta: &MetaStore, digest: &Digest) -> Result<Vec<PlacementDescriptor>, SyncError> {
    let artifact = ArtifactDigest::from_sha256(digest.as_str()).expect("a journal digest is canonical sha256 hex");
    let placements = meta.blob_placements(&artifact)?;
    Ok(placements.iter().map(PlacementDescriptor::from).collect())
}

/// Recovers missing blobs from the durable journal tail after [`BLOB_VIEW`].
///
/// The journal size bounds ranged allocation, and storage receives only verified whole blobs.
///
/// # Errors
/// The same failures as [`pull_referenced`], a terminal ranged-fetch loss ([`SyncError::BlobFetchFailed`]),
/// or a store error reading the journal tail or a blob's placements.
pub async fn pull_outstanding<T: BlobTransport>(
    sources: &BlobSources<'_, T>,
    meta: &MetaStore,
    blobs: &BlobStorage,
    batch: NonZeroUsize,
    concurrency: NonZeroUsize,
) -> Result<BlobPlaneReport, SyncError> {
    let reachable: BTreeSet<String> = sources.delegates.keys().cloned().collect();
    let referenced = referenced_over_tail(meta, batch)?;
    let mut simple = Vec::new();
    let mut ranged = Vec::new();
    for (digest, size) in &referenced {
        if blobs.head(digest).await?.is_some() {
            repair_local_placement(meta, digest)?;
            continue;
        }
        let descriptors = placement_descriptors(meta, digest)?;
        match classify_blob(&descriptors, sources.local_dc, &reachable) {
            BlobDisposition::Defer => {}
            BlobDisposition::Ranged(dcs) => ranged.push((digest.clone(), *size, dcs)),
            BlobDisposition::Whole => simple.push((digest.clone(), *size)),
        }
    }
    let mut report = pull_referenced(sources.simple, blobs, meta, &simple, concurrency).await?;
    for (digest, size, plan) in ranged {
        pull_one_ranged(meta, blobs, sources, &digest, size, &plan, &mut report).await?;
    }
    Ok(report)
}

/// Source exhaustion leaves the blob pending; length and verification failures fail closed.
async fn pull_one_ranged<T: BlobTransport>(
    meta: &MetaStore,
    blobs: &BlobStorage,
    sources: &BlobSources<'_, T>,
    digest: &Digest,
    size: u64,
    dcs: &[String],
    report: &mut BlobPlaneReport,
) -> Result<(), SyncError> {
    let transports: Vec<&T> = dcs.iter().filter_map(|dc| sources.delegates.get(dc)).collect();
    // Bound reassembly with the trusted journal size, never a peer advertisement.
    let total_length = usize::try_from(size).expect("a blob fits addressable memory");
    let reason = match pull_ranged_blob(&transports, digest, total_length).await {
        Ok(bytes) => {
            commit_blob(blobs, meta, digest, size, bytes.to_vec()).await?;
            report.fetched += 1;
            return Ok(());
        }
        Err(PullError::Exhausted { .. }) => {
            report.pending += 1;
            return Ok(());
        }
        Err(PullError::Piece(_)) => "range_length_mismatch",
        Err(PullError::Reassembly(_)) => "reassembly_failed",
    };
    Err(SyncError::BlobFetchFailed {
        reason,
        digest: digest.as_str().to_owned(),
    })
}

/// Omits unparseable digests from pulls; [`advance_blob_frontier`] still holds the frontier below them.
fn referenced_over_tail(meta: &MetaStore, batch: NonZeroUsize) -> Result<Vec<(Digest, u64)>, SyncError> {
    let frontier = meta.view_frontier(BLOB_VIEW)?.unwrap_or(0);
    let (_authority, records) = meta.journal_page_after(frontier, batch.get())?;
    let mut referenced = Vec::new();
    for record in &records {
        for blob in &record.blobs {
            if let Some(digest) = Digest::from_hex(&blob.sha256) {
                referenced.push((digest, blob.size));
            }
        }
    }
    Ok(referenced)
}

/// Recomputes [`BLOB_VIEW`] from a bounded journal scan and current blob availability.
///
/// A reachable read-through peer satisfies a local miss.
///
/// # Errors
/// Returns a store error reading the journal, probing blob presence, reading a blob's placements, or
/// writing the frontier.
pub async fn advance_blob_frontier(
    meta: &MetaStore,
    blobs: &BlobStorage,
    batch: NonZeroUsize,
    local_dc: &str,
    reachable: &BTreeSet<String>,
) -> Result<u64, SyncError> {
    let frontier = meta.view_frontier(BLOB_VIEW)?.unwrap_or(0);
    let (_authority, records) = meta.journal_page_after(frontier, batch.get())?;
    // Never advance beyond the highest serial examined in this bounded batch.
    let batch_end = records.last().map_or(frontier, |record| record.serial);
    let mut referenced = Vec::new();
    for record in &records {
        for blob in &record.blobs {
            let (present_locally, served_by_peer) = match Digest::from_hex(&blob.sha256) {
                Some(digest) if blobs.head(&digest).await?.is_some() => (true, false),
                Some(digest) => (
                    false,
                    deferred_to_peer(&placement_descriptors(meta, &digest)?, local_dc, reachable),
                ),
                // An unparseable digest cannot be served and holds the frontier closed.
                None => (false, false),
            };
            referenced.push(ReferencedBlob {
                serial: record.serial,
                digest: blob.sha256.clone(),
                // #826 pages lack placement data; #830 will replace this default with advertised state.
                availability: PlacementAvailability::Verified,
                present_locally,
                served_by_peer,
            });
        }
    }
    let BlobAvailability { serial, .. } = blob_availability(batch_end, &referenced);
    meta.set_view_frontier(BLOB_VIEW, serial)?;
    Ok(serial)
}

/// Repairs the placement when byte commit succeeded but the later placement write failed.
fn repair_local_placement(meta: &MetaStore, digest: &Digest) -> Result<(), SyncError> {
    match meta.get_artifact_placement(digest.as_str())? {
        Some(placement) if placement.availability.is_local() => {}
        Some(_) => {
            apply_placement_event(meta, digest.as_str(), PlacementEvent::BytesVerified)?;
        }
        None => {
            record_artifact_placement(meta, digest.as_str(), ArtifactSource::Proxy, true)?;
        }
    }
    Ok(())
}

async fn commit_blob(
    blobs: &BlobStorage,
    meta: &MetaStore,
    digest: &Digest,
    size: u64,
    bytes: Vec<u8>,
) -> Result<(), SyncError> {
    let actual = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if actual != size {
        return Err(SyncError::BlobSizeMismatch {
            digest: digest.as_str().to_owned(),
            expected: size,
            actual,
        });
    }
    let mut write = blobs.begin().await?;
    write.write_chunk(Bytes::from(bytes)).await?;
    write.commit(digest).await?;
    // Publish placement after durable bytes; the primary can resupply replicas recorded as `Proxy`.
    record_artifact_placement(meta, digest.as_str(), ArtifactSource::Proxy, true)?;
    Ok(())
}
