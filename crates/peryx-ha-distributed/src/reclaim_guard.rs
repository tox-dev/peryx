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

    release_absent_expired_guards(meta, blobs, now)?;
    let guard = ReclaimGuard {
        expires_at_unix: now.saturating_add(RECLAIM_GUARD_LEASE_SECS),
    };
    let armed = loop {
        let serial = meta.reclaim_guard_serial()?;
        let live_digests = scan_references().map_err(OrphanPurgeError::References)?;
        let digests = candidates
            .iter()
            .filter(|candidate| !live_digests.contains(candidate.digest.as_str()))
            .map(|candidate| candidate.digest.as_str())
            .collect::<Vec<_>>();
        match meta.compare_and_arm_reclaim_guards(&digests, serial, now, guard)? {
            ReclaimGuardArm::SerialChanged => {}
            ReclaimGuardArm::Armed(armed) => break armed.into_iter().collect::<BTreeSet<_>>(),
        }
    };
    let selected = candidates
        .into_iter()
        .filter(|candidate| armed.contains(candidate.digest.as_str()))
        .collect::<Vec<_>>();
    for candidate in &selected {
        blobs
            .blocking()
            .delete(&candidate.digest)
            .map_err(|error| OrphanPurgeError::Blob {
                operation: "delete orphaned blob",
                reason: error.to_string(),
            })?;
        meta.compare_and_disarm_reclaim_guard(candidate.digest.as_str(), guard)?;
    }
    Ok(report(selected))
}

fn release_absent_expired_guards(meta: &MetaStore, blobs: &BlobStorage, now: i64) -> Result<(), OrphanPurgeError> {
    for (encoded, guard) in meta.reclaim_guards()? {
        if !guard.is_expired_at(now) {
            continue;
        }
        let digest = Digest::from_hex(&encoded).ok_or_else(|| OrphanPurgeError::Blob {
            operation: "read orphan reclaim guard",
            reason: format!("invalid SHA-256 digest {encoded:?}"),
        })?;
        let absent = blobs
            .blocking()
            .head(&digest)
            .map_err(|error| OrphanPurgeError::Blob {
                operation: "inspect guarded blob",
                reason: error.to_string(),
            })?
            .is_none();
        if absent {
            meta.compare_and_disarm_reclaim_guard(&encoded, guard)?;
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
