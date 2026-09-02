//! Commits verified blobs before recording their local placement. Frontier advancement remains separate,
//! so metadata stays unreadable until its blobs are locally present or available through read-through.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::num::NonZeroUsize;

use bytes::Bytes;
use futures_util::FutureExt as _;
use peryx_ha::{ArtifactSource, BlobCommit, PlacementEvent};
use peryx_identity::ArtifactDigest;
use peryx_storage::blob::{BlobStorage, Digest};
use peryx_storage::meta::MetaStore;

use crate::blob::BlobTransport;
use crate::blob_availability::{BlobAvailability, ReferencedBlob, blob_availability};
use crate::blob_fetch::{FetchOutcome, FetchReport, fetch_missing};
use crate::blob_placement::{FetchPlan, plan_blob_fetch};
use crate::blob_pull::{ChunkFailure, ChunkUnavailable};
use crate::blob_stage::{DEFAULT_RANGED_PULL_BUDGET, StagedPullError, pull_blob_staged};
use crate::error::SyncError;
use crate::protocol::{PlacementAvailability, PlacementDescriptor};
use crate::{TransportError, apply_placement_event, record_artifact_placement};

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

/// Byte-serving evidence gathered during one blob pull pass.
#[derive(Debug, Default)]
pub struct PeerBlobEvidence(BTreeSet<String>);

enum BlobDisposition {
    Defer(Vec<String>),
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
        BlobDisposition::Defer(sources)
    } else if sources.len() >= 2 {
        BlobDisposition::Ranged(sources)
    } else {
        BlobDisposition::Whole
    }
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
    Ok(
        pull_outstanding_with_evidence(sources, meta, blobs, batch, concurrency, &mut Vec::new())
            .await?
            .0,
    )
}

/// Pulls outstanding blobs and returns evidence for blobs served by a peer.
///
/// # Errors
/// Returns a store, transport, placement, or blob verification error.
pub async fn pull_outstanding_with_evidence<T: BlobTransport>(
    sources: &BlobSources<'_, T>,
    meta: &MetaStore,
    blobs: &BlobStorage,
    batch: NonZeroUsize,
    concurrency: NonZeroUsize,
    committed: &mut Vec<BlobCommit>,
) -> Result<(BlobPlaneReport, PeerBlobEvidence), SyncError> {
    let reachable: BTreeSet<String> = sources.delegates.keys().cloned().collect();
    let tail = referenced_over_tail(meta, batch)?;
    let mut simple = Vec::new();
    let mut ranged = Vec::new();
    let mut deferred = Vec::new();
    let mut placed = BTreeSet::new();
    for (digest, size) in &tail.referenced {
        if blobs.head(digest).await?.is_some() {
            if repair_local_placement(meta, digest)? {
                placed.insert(digest.as_str().to_owned());
            }
            continue;
        }
        let descriptors = placement_descriptors(meta, digest)?;
        match classify_blob(&descriptors, sources.local_dc, &reachable) {
            BlobDisposition::Defer(dcs) => deferred.push((digest.clone(), *size, dcs)),
            BlobDisposition::Ranged(dcs) => ranged.push((digest.clone(), *size, dcs)),
            BlobDisposition::Whole => simple.push((digest.clone(), *size)),
        }
    }
    let attempted = simple
        .iter()
        .map(|(digest, _)| digest.clone())
        .chain(ranged.iter().map(|(digest, _, _)| digest.clone()))
        .collect::<Vec<_>>();
    let pulled = pull_batch(sources, meta, blobs, &simple, ranged, concurrency).await;
    // A pull reports how many blobs it moved, not which, and this runs whether or not it finished: a
    // pass that fails after committing some blobs has already made those bytes local, and the next pass
    // finds them present and reports nothing, so a commit dropped here is never offered again.
    for digest in attempted {
        if blobs.head(&digest).await?.is_some() {
            placed.insert(digest.as_str().to_owned());
        }
    }
    *committed = tail.commits(&placed);
    let mut report = pulled?;
    let mut served_by_peer = BTreeSet::new();
    for (digest, size, dcs) in deferred {
        if probe_deferred_blob(sources, &digest, size, &dcs).await? {
            served_by_peer.insert(digest.as_str().to_owned());
        } else {
            report.pending += 1;
        }
    }
    Ok((report, PeerBlobEvidence(served_by_peer)))
}

async fn pull_batch<T: BlobTransport>(
    sources: &BlobSources<'_, T>,
    meta: &MetaStore,
    blobs: &BlobStorage,
    simple: &[(Digest, u64)],
    ranged: Vec<(Digest, u64, Vec<String>)>,
    concurrency: NonZeroUsize,
) -> Result<BlobPlaneReport, SyncError> {
    let mut report = pull_referenced(sources.simple, blobs, meta, simple, concurrency)
        .boxed()
        .await?;
    for (digest, size, plan) in ranged {
        pull_one_ranged(meta, blobs, sources, &digest, size, &plan, &mut report)
            .boxed()
            .await?;
    }
    Ok(report)
}

async fn probe_deferred_blob<T: BlobTransport>(
    sources: &BlobSources<'_, T>,
    digest: &Digest,
    size: u64,
    dcs: &[String],
) -> Result<bool, SyncError> {
    let mut terminal = None;
    let mut retryable = false;
    for dc in dcs {
        match sources.delegates[dc].blob_size(digest).await {
            Ok(Some(actual)) if actual == size => return Ok(true),
            Ok(Some(_)) => terminal = Some("blob_size_mismatch"),
            Ok(None) => terminal = Some("blob_not_found"),
            Err(error) if error.is_retryable() => retryable = true,
            Err(TransportError::BadStatus { status: 404 }) => terminal = Some("blob_route_unavailable"),
            Err(error) => terminal = error.terminal_reason(),
        }
    }
    if retryable {
        Ok(false)
    } else {
        Err(SyncError::BlobFetchFailed {
            reason: terminal.unwrap_or("blob_not_found"),
            digest: digest.as_str().to_owned(),
        })
    }
}

/// Source exhaustion leaves the blob pending; length and verification failures fail closed. The bytes
/// reach storage through a staged, budgeted pipeline, so peak memory follows the range size rather than
/// the blob's.
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
    // Bound the transfer with the trusted journal size, never a peer advertisement.
    let total_length = usize::try_from(size).expect("a blob fits addressable memory");
    let artifact = ArtifactDigest::from_sha256(digest.as_str()).expect("a journal digest is canonical sha256 hex");
    let catalog = meta.blob_chunk_digest(&artifact)?;
    let pull = pull_blob_staged(
        blobs,
        &transports,
        digest,
        total_length,
        catalog.as_ref(),
        DEFAULT_RANGED_PULL_BUDGET,
    )
    .await;
    let reason = match pull {
        Ok(_) => {
            // Publish placement after durable bytes; the primary can resupply replicas recorded as `Proxy`.
            record_artifact_placement(meta, digest.as_str(), ArtifactSource::Proxy, true)?;
            report.fetched += 1;
            return Ok(());
        }
        Err(StagedPullError::Stage(error)) => return Err(SyncError::Blob(error)),
        Err(StagedPullError::DigestMismatch { .. }) => "blob_digest_mismatch",
        Err(StagedPullError::RangeUnavailable(unavailable)) => {
            let Some(reason) = terminal_range_reason(&unavailable) else {
                report.pending += 1;
                return Ok(());
            };
            reason
        }
    };
    Err(SyncError::BlobFetchFailed {
        reason,
        digest: digest.as_str().to_owned(),
    })
}

/// A source that failed at the transport may recover, so the blob stays pending. A source that served the
/// wrong length or wrong chunk bytes will serve them again, so the pass fails closed.
fn terminal_range_reason(unavailable: &ChunkUnavailable) -> Option<&'static str> {
    let mut reason = None;
    for (_, failure) in &unavailable.failures {
        match failure {
            ChunkFailure::Transport(_) => return None,
            ChunkFailure::WrongLength { .. } => reason = Some("range_length_mismatch"),
            ChunkFailure::DigestMismatch => reason = Some("chunk_digest_mismatch"),
        }
    }
    reason
}

/// Omits unparseable digests from pulls; [`advance_blob_frontier`] still holds the frontier below them.
/// The blobs the journal tail references, each carrying the metadata keys its own record committed.
///
/// A journal record holds the mutations and the blob references of one serial together, so a replica
/// already stores the association between a blob and the resources whose records named it. Nothing has
/// to replicate it, derive it from a locator this node may not hold, or carry it in the pull protocol.
struct ReferencedTail {
    referenced: Vec<(Digest, u64)>,
    keys: BTreeMap<String, BTreeSet<String>>,
}

impl ReferencedTail {
    /// Pairs each newly local digest with the keys to retire for it, empty when the tail names none.
    fn commits(&self, placed: &BTreeSet<String>) -> Vec<BlobCommit> {
        placed
            .iter()
            .map(|digest| BlobCommit {
                digest: digest.clone(),
                keys: self.keys.get(digest).into_iter().flatten().cloned().collect(),
            })
            .collect()
    }
}

fn referenced_over_tail(meta: &MetaStore, batch: NonZeroUsize) -> Result<ReferencedTail, SyncError> {
    let frontier = meta.view_frontier(BLOB_VIEW)?.unwrap_or(0);
    let (_authority, records) = meta.journal_page_after(frontier, batch.get())?;
    let mut tail = ReferencedTail {
        referenced: Vec::new(),
        keys: BTreeMap::new(),
    };
    for record in &records {
        let named = record
            .blobs
            .iter()
            .filter_map(|blob| Digest::from_hex(&blob.sha256).map(|digest| (digest, blob.size)));
        for (digest, size) in named {
            tail.keys
                .entry(digest.as_str().to_owned())
                .or_default()
                .extend(record.mutations.iter().map(|mutation| mutation.key().to_owned()));
            tail.referenced.push((digest, size));
        }
    }
    Ok(tail)
}

/// Recomputes [`BLOB_VIEW`] from a bounded journal scan and local blob availability.
///
/// Configured peer reachability is not proof that a peer serves a locally absent blob.
///
/// # Errors
/// Returns a store error reading the journal, probing blob presence, or writing the frontier.
pub async fn advance_blob_frontier(
    meta: &MetaStore,
    blobs: &BlobStorage,
    batch: NonZeroUsize,
    _local_dc: &str,
    _reachable: &BTreeSet<String>,
) -> Result<u64, SyncError> {
    advance_blob_frontier_with_evidence(meta, blobs, batch, &PeerBlobEvidence::default()).await
}

/// Recomputes [`BLOB_VIEW`] using byte-serving evidence from the current pull pass.
///
/// # Errors
/// Returns a store error reading the journal, probing blob presence, or writing the frontier.
pub async fn advance_blob_frontier_with_evidence(
    meta: &MetaStore,
    blobs: &BlobStorage,
    batch: NonZeroUsize,
    served_by_peer: &PeerBlobEvidence,
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
                Some(digest) => (false, served_by_peer.0.contains(digest.as_str())),
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

/// Repairs the placement when byte commit succeeded but the later placement write failed, reporting
/// whether the row moved to local, which is what a derived view reading availability answers from.
///
/// The absent case records rather than reads. This runs for a digest whose bytes this replica has just
/// confirmed, so the caller holds the observation the projection lacks, and `Proxy` is the replica's
/// true source: the bytes came from the primary, which can resupply them. It repairs only what the
/// journal named, so it is no general backstop for the rows
/// [#2141](https://github.com/tox-dev/peryx/issues/2141) lists.
fn repair_local_placement(meta: &MetaStore, digest: &Digest) -> Result<bool, SyncError> {
    match meta.get_artifact_placement(digest.as_str())? {
        Some(placement) if placement.availability.is_local() => Ok(false),
        Some(_) => {
            apply_placement_event(meta, digest.as_str(), PlacementEvent::BytesVerified)?;
            Ok(true)
        }
        None => {
            record_artifact_placement(meta, digest.as_str(), ArtifactSource::Proxy, true)?;
            Ok(true)
        }
    }
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
