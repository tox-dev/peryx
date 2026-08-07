//! The async whole-blob plane: pull the blobs a metadata page referenced but this replica lacks, commit
//! their verified bytes, and record their local presence.
//!
//! [`sync_metadata`](crate::replica::Replica::sync_metadata) commits a page's metadata ahead of its
//! bytes and hands back the referenced `(digest, size)` set. This drives that set to ground: it skips the
//! blobs already present, pulls the rest whole through [`fetch_missing`], commits each verified blob, and
//! flips its [`ArtifactPlacement`](peryx_storage::meta::ArtifactPlacement) to
//! [`Local`](peryx_storage::meta::ByteAvailability::Local). It touches no metadata and moves no frontier:
//! advancing [`BLOB_VIEW`] once the bytes are down is the loop's job, gated by the blob-availability
//! frontier so a serial stays out of the readable frontier until its blobs are here.

use std::collections::{BTreeSet, HashMap};
use std::num::NonZeroUsize;

use bytes::Bytes;
use peryx_identity::ArtifactDigest;
use peryx_storage::blob::{BlobStorage, Digest};
use peryx_storage::meta::{ArtifactSource, MetaStore, PlacementEvent};

use crate::blob::BlobTransport;
use crate::blob_availability::{BlobAvailability, ReferencedBlob, blob_availability};
use crate::blob_fetch::{FetchOutcome, FetchReport, fetch_missing};
use crate::blob_placement::{FetchPlan, plan_blob_fetch};
use crate::blob_pull::{PullError, pull_ranged_blob};
use crate::error::SyncError;
use crate::protocol::{PlacementAvailability, PlacementDescriptor};

/// The derived-view name whose frontier tracks how far a replica's metadata is backed by blobs it holds.
///
/// The loop advances this with [`set_view_frontier`](peryx_storage::meta::MetaStore::set_view_frontier)
/// and the driver's readable frontier gates visibility on it alongside the search view, so a metadata
/// serial is not exposed until its blobs are present.
pub const BLOB_VIEW: &str = "blob";

/// What one blob-plane pass made of the blobs a page referenced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobPlaneReport {
    /// How many absent blobs this pass fetched, committed, and marked local.
    pub fetched: usize,
    /// How many blobs a retryable loss left for a later pass, so the caller retries without failing.
    pub pending: usize,
}

/// Pull every referenced blob this replica lacks, commit it, and record it local.
///
/// A blob already present is not re-fetched, but its local placement is verified and repaired first: the
/// commit publishes bytes before recording placement, so a placement write that failed after the bytes
/// landed leaves the record reading remote-only, unavailable, or missing, and a plain skip would strand
/// that. The absent ones are pulled whole through
/// [`fetch_missing`], which digest-verifies each in transport, and each fetched blob is committed under
/// its digest and its placement flipped to local. A retryable loss leaves the affected blobs
/// [pending](BlobPlaneReport::pending) for the next pass; a terminal loss on a whole-blob fetch is a real
/// failure the caller records and retries, not a silent skip — the frontier holds that serial back until
/// the byte lands regardless.
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
    // Committing off `absent` keeps each blob paired with the size its reference declared. A retryable
    // pass leaves some absent blobs unfetched, so a digest with no bytes here is simply skipped.
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

/// The transports one blob-plane pass may draw bytes from.
///
/// `simple` is the whole-blob transport every single-source blob falls through to, the digest-verified
/// whole-blob path the plane has always used. `delegates` maps a data center to its ranged transport and
/// `local_dc` names this replica's own data center, so a same-datacenter source is preferred. The keys of
/// `delegates` are the peer data centers this replica can reach, and a blob's advertised placements decide
/// how it is drawn:
///
/// - Placed only on reachable peers, none in `local_dc`: deferred to cross-DC read-through, which fills a
///   public download from the peer that holds it. The replica leaves the bytes absent rather than drawing
///   them eagerly.
/// - Placed locally with two or more reachable copies: pulled as a multi-source ranged fetch across the
///   peers that hold it.
/// - Anything else, including a blob no descriptor places on a reachable peer: drawn whole over `simple`.
///
/// With an empty `delegates` and an empty `local_dc` every blob takes the whole-blob path, the behavior a
/// replica keeps until it resolves its own data center and its peers.
pub struct BlobSources<'a, T> {
    /// The whole-blob transport for a single-source blob.
    pub simple: &'a T,
    /// The per-data-center ranged transports a multi-source blob draws from, keyed by the reachable peer
    /// data center each one serves.
    pub delegates: &'a HashMap<String, T>,
    /// This replica's own data center, preferred when a blob has a local placement.
    pub local_dc: &'a str,
}

/// How the blob puller should source one absent blob, decided from its advertised placements.
enum BlobDisposition {
    /// The policy places the blob only on reachable peers, none in `local_dc`, so it is left absent for
    /// cross-DC read-through to fill on a public download rather than drawn eagerly.
    Defer,
    /// The blob has two or more reachable placements: drawn as ranges across the peers that hold it.
    Ranged(Vec<String>),
    /// A single or no reachable placement, or a blob placed locally: drawn whole over `simple`.
    Whole,
}

/// Classify one blob from its advertised `descriptors`, `local_dc`, and the `reachable` peer data centers.
///
/// A verified placement in `local_dc` makes this replica a holder, so the blob is pulled (whole, or ranged
/// when two or more reachable peers also hold it) rather than deferred. With no local placement but a
/// verified placement on a reachable peer the blob is deferred to read-through. Everything else — no
/// verified placement, or one no reachable peer can serve — falls to the whole-blob path over the upstream
/// `simple` transport.
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

/// Whether the policy defers `digest` to cross-DC read-through: placed only on a reachable peer, none in
/// `local_dc`. The frontier advance uses this to let a deferred blob through, since read-through serves it.
fn deferred_to_peer(descriptors: &[PlacementDescriptor], local_dc: &str, reachable: &BTreeSet<String>) -> bool {
    matches!(classify_blob(descriptors, local_dc, reachable), BlobDisposition::Defer)
}

/// The placement descriptors a peer advertises for `digest`, read from the local ledger. A journal digest
/// is a canonical lowercase-hex sha256, exactly what [`ArtifactDigest::from_sha256`] accepts.
fn placement_descriptors(meta: &MetaStore, digest: &Digest) -> Result<Vec<PlacementDescriptor>, SyncError> {
    let artifact = ArtifactDigest::from_sha256(digest.as_str()).expect("a journal digest is canonical sha256 hex");
    let placements = meta.blob_placements(&artifact)?;
    Ok(placements.iter().map(PlacementDescriptor::from).collect())
}

/// Pull every blob the journal tail after the current [`BLOB_VIEW`] frontier references that this replica
/// still lacks.
///
/// A multi-placement blob is drawn as ranges across the peers that hold it; a single-source blob is drawn
/// whole. This is the self-healing counterpart to [`pull_referenced`]: it derives the outstanding set from
/// durable state — the same bounded journal tail [`advance_blob_frontier`] examines — rather than the
/// page a single cycle produced. So a blob that back-pressured or failed on an earlier cycle is retried
/// on a later one, and a restarted replica re-derives the set from the journal with no lost in-memory
/// pending.
///
/// Each absent blob is classified from its advertised placements by [`classify_blob`]: one placed only on
/// reachable peers is deferred to cross-DC read-through and left absent; one placed locally with two or
/// more reachable copies is fetched as a ranged multi-source pull ([`pull_ranged_blob`]) that falls
/// through source to source and verifies the reassembled whole; every other blob defers to
/// [`pull_referenced`] over [`simple`](BlobSources::simple). The trusted journal size bounds the ranged
/// reassembly's pre-allocation, never a peer advertisement. Only verified whole blobs are committed, so a
/// ranged fetch that a peer corrupts fails closed rather than landing bad bytes.
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
            // A peer datacenter holds it; read-through fills a public download rather than a whole-pull.
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

/// Drive one ranged multi-source pull and fold its result into `report`. A source exhaustion leaves the
/// blob [pending](BlobPlaneReport::pending) for a later pass; a length or verification failure is a
/// terminal [`SyncError::BlobFetchFailed`] the caller records and retries.
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
    // The trusted journal size bounds the reassembly pre-allocation, never a peer advertisement.
    let total_length = usize::try_from(size).expect("a blob fits addressable memory");
    let reason = match pull_ranged_blob(&transports, digest, total_length).await {
        Ok(bytes) => {
            commit_blob(blobs, meta, digest, size, bytes.to_vec()).await?;
            report.fetched += 1;
            return Ok(());
        }
        // Every source lost the range: retryable, so the blob is left for a later pass.
        Err(PullError::Exhausted { .. }) => {
            report.pending += 1;
            return Ok(());
        }
        // A source served the wrong number of bytes for a range, or the reassembled whole did not verify:
        // fail closed rather than committing bytes a peer corrupted.
        Err(PullError::Piece(_)) => "range_length_mismatch",
        Err(PullError::Reassembly(_)) => "reassembly_failed",
    };
    Err(SyncError::BlobFetchFailed {
        reason,
        digest: digest.as_str().to_owned(),
    })
}

/// The `(digest, size)` set every journal record after the current [`BLOB_VIEW`] frontier references,
/// bounded to `batch` records. A digest that cannot parse is left out: it cannot be pulled, and
/// [`advance_blob_frontier`] holds the frontier below it regardless.
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

/// Advance the [`BLOB_VIEW`] frontier as far as this replica's local blobs allow, and return the serial
/// it reached.
///
/// The frontier re-derives from durable state alone: it reads the journal tail after the current
/// [`BLOB_VIEW`] frontier, bounded to `batch` records, and folds each referenced blob's local presence
/// through [`blob_availability`]. A blob absent locally but placed only on a reachable peer — one
/// [`deferred_to_peer`], with `local_dc` and the `reachable` peer data centers deciding it — is serveable
/// through cross-DC read-through, so it passes the frontier rather than pinning it the way a plain local
/// miss does. No separate durable cursor is kept, so a restart recomputes the same serial from the journal
/// and the blob store. The `batch` bound caps the work: a persistently-missing blob pins the frontier at
/// that serial at `O(batch)` cost per pass rather than an unbounded rescan, and the frontier simply
/// advances one batch at a time when the plane keeps up. It moves only the frontier, committing no bytes
/// and no metadata.
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
    // The bounded batch is the ceiling this pass can reach: the frontier advances at most to the highest
    // serial examined, so serials past the batch are left for a later pass rather than assumed backed.
    let batch_end = records.last().map_or(frontier, |record| record.serial);
    let mut referenced = Vec::new();
    for record in &records {
        for blob in &record.blobs {
            let (present_locally, served_by_peer) = match Digest::from_hex(&blob.sha256) {
                Some(digest) if blobs.head(&digest).await?.is_some() => (true, false),
                // Absent locally: it still passes the frontier when a reachable peer holds it, since
                // read-through serves that public download; otherwise it holds the frontier closed.
                Some(digest) => (
                    false,
                    deferred_to_peer(&placement_descriptors(meta, &digest)?, local_dc, reachable),
                ),
                // A journal digest that cannot parse cannot be served, so it holds the frontier closed.
                None => (false, false),
            };
            referenced.push(ReferencedBlob {
                serial: record.serial,
                digest: blob.sha256.clone(),
                // #830: a whole-blob #826 page advertises no placement, so a referenced blob is treated as
                // Verified and only presence or peer deferral gates it; when placement descriptors ride in
                // the page, the real advertised availability replaces this.
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

/// Repair the local placement of a blob whose bytes are already present but whose placement does not yet
/// record them [`Local`](peryx_storage::meta::ByteAvailability::Local).
///
/// [`commit_blob`] publishes bytes before recording placement, so a placement write that failed after the
/// commit leaves a local blob whose placement still reads remote-only, unavailable, or missing. A retry
/// finds the bytes present and would skip the blob; this rewrites the record instead, so reconciliation
/// and availability see the copy that is actually here. A placement already local is left untouched.
fn repair_local_placement(meta: &MetaStore, digest: &Digest) -> Result<(), SyncError> {
    match meta.get_artifact_placement(digest.as_str())? {
        Some(placement) if placement.availability.is_local() => {}
        // A placement exists but reads absent: flip it local, keeping the recorded source.
        Some(_) => {
            meta.apply_placement_event(digest.as_str(), PlacementEvent::BytesVerified)?;
        }
        // No placement at all: record it under the resupply-able source `commit_blob` records a freshly
        // pulled replicated blob under.
        None => {
            meta.record_artifact_placement(digest.as_str(), ArtifactSource::Proxy, true)?;
        }
    }
    Ok(())
}

/// Commit one verified blob's bytes under its digest and record it locally present.
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
    // The commit now returns a durable placement receipt, proof the bytes are digest-verified, synced, and
    // atomically published past the filesystem boundary. Quorum and cross-DC placement consume that
    // evidence downstream; here the local durable copy only needs recording as present.
    write.commit(digest).await?;
    // A replicated blob is resupply-able from the primary, so it records under the resupply-able source
    // (`Proxy` projects `RemoteOnly` when its bytes are absent, not `Unavailable`). A whole-blob #826 page
    // carries no per-artifact origin, so the artifact's true source is unknown here; #830 placement
    // descriptors riding in the page will supply it. `present = true` flips availability to `Local`.
    meta.record_artifact_placement(digest.as_str(), ArtifactSource::Proxy, true)?;
    Ok(())
}
