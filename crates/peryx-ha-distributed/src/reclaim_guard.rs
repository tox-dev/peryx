use std::collections::BTreeSet;
use std::convert::Infallible;
use std::path::PathBuf;

use peryx_ha::{ReclaimGuard, ReclaimGuardArm, ReclaimGuardStore};
use peryx_storage::blob::{BlobStorage, Digest};
use peryx_storage::meta::{MetaError, MetaStore};

pub const RECLAIM_GUARD_LEASE_SECS: i64 = 300;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanBlob {
    pub digest: String,
    pub bytes: u64,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanPurgeReport {
    pub blobs: Vec<OrphanBlob>,
    pub bytes: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum OrphanPurgeError {
    #[error("scan metadata blob references: {0}")]
    References(String),
    #[error("{operation}: {reason}")]
    Blob { operation: &'static str, reason: String },
    #[error(transparent)]
    Store(#[from] MetaError),
}

/// # Errors
/// Returns metadata, blob, or reference-scan failures.
pub fn purge_orphaned_blobs(
    meta: &MetaStore,
    blobs: &BlobStorage,
    confirmed: bool,
    now: i64,
    mut scan_references: impl FnMut() -> Result<BTreeSet<String>, String>,
) -> Result<OrphanPurgeReport, OrphanPurgeError> {
    let live_digests = scan_references().map_err(OrphanPurgeError::References)?;
    let candidates = orphan_candidates(blobs, &live_digests)?;
    if !confirmed {
        let live_digests = scan_references().map_err(OrphanPurgeError::References)?;
        return Ok(report(
            candidates
                .into_iter()
                .filter(|candidate| !live_digests.contains(candidate.digest.as_str())),
        ));
    }

    release_expired_guards(meta, now)?;
    let guard = ReclaimGuard {
        expires_at_unix: now.saturating_add(RECLAIM_GUARD_LEASE_SECS),
    };
    // A reference commit between the scan and the arm moves the revision, which retires the scan
    // rather than guarding a digest that is no longer orphaned.
    let armed = loop {
        let revision = meta.reference_revision()?;
        let live_digests = scan_references().map_err(OrphanPurgeError::References)?;
        let digests = candidates
            .iter()
            .filter(|candidate| !live_digests.contains(candidate.digest.as_str()))
            .map(|candidate| candidate.digest.as_str())
            .collect::<Vec<_>>();
        let ReclaimGuardArm::Armed(armed) = meta.compare_and_arm_reclaim_guards(&digests, revision, now, guard)? else {
            continue;
        };
        break armed.into_iter().collect::<BTreeSet<_>>();
    };
    let selected = candidates
        .into_iter()
        .filter(|candidate| armed.contains(candidate.digest.as_str()))
        .collect::<Vec<_>>();
    for candidate in &selected {
        // The placement goes before the bytes. An interruption between the two then leaves a digest
        // whose row claims nothing while the bytes survive, which understates what this node holds; the
        // other order leaves a row promising bytes that are gone, and a later reference to the same
        // digest would read that promise and offer content no read can serve.
        meta.delete_artifact_placement(candidate.digest.as_str())?;
        if let Err(error) = blobs.blocking().delete(&candidate.digest) {
            return Err(OrphanPurgeError::Blob {
                operation: "delete orphaned blob",
                reason: error.to_string(),
            });
        }
        meta.compare_and_disarm_reclaim_guard(candidate.digest.as_str(), guard)?;
    }
    Ok(report(selected))
}

/// A lapsed lease proves its collector no longer holds the blob, whether or not the bytes survived
/// the purge that armed it, so the row goes regardless of what the store reports.
fn release_expired_guards(meta: &MetaStore, now: i64) -> Result<(), OrphanPurgeError> {
    for (digest, guard) in meta.reclaim_guards()? {
        if guard.is_expired_at(now) {
            meta.compare_and_disarm_reclaim_guard(&digest, guard)?;
        }
    }
    Ok(())
}

fn orphan_candidates(blobs: &BlobStorage, referenced: &BTreeSet<String>) -> Result<Vec<Candidate>, OrphanPurgeError> {
    let mut candidates = Vec::new();
    blobs
        .blocking()
        .visit(|entry| {
            if let Some(digest) = entry.digest
                && !referenced.contains(digest.as_str())
            {
                candidates.push(Candidate {
                    digest,
                    bytes: entry.bytes,
                    path: entry.path,
                });
            }
            Ok::<(), Infallible>(())
        })
        .map_err(|error| OrphanPurgeError::Blob {
            operation: "scan orphaned blob files",
            reason: error.to_string(),
        })?;
    Ok(candidates)
}

fn report(candidates: impl IntoIterator<Item = Candidate>) -> OrphanPurgeReport {
    let blobs = candidates
        .into_iter()
        .map(|candidate| OrphanBlob {
            digest: candidate.digest.as_str().to_owned(),
            bytes: candidate.bytes,
            path: candidate.path,
        })
        .collect::<Vec<_>>();
    OrphanPurgeReport {
        bytes: blobs.iter().map(|blob| blob.bytes).sum(),
        blobs,
    }
}

struct Candidate {
    digest: Digest,
    bytes: u64,
    path: PathBuf,
}
