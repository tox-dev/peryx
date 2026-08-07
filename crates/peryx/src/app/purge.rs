//! Cache purging: per-project removal, orphaned-blob collection, and the reference scan.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::PathBuf;

use anyhow::Context as _;
use peryx_driver::serving::PurgeReport;
use peryx_storage::blob::{BlobStorage, Digest};
use peryx_storage::meta::MetaStore;

use super::CacheStores;
use crate::cli::{CachePurgeOrphanedBlobsArgs, CachePurgeProjectArgs};
use crate::config::Config;

pub(super) fn purge_project(
    config: &Config,
    stores: &CacheStores,
    args: &CachePurgeProjectArgs,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let ecosystem = config
        .indexes
        .iter()
        .find(|index| index.name == args.index)
        .context(format!("unknown index {:?}", args.index))?
        .ecosystem;
    let driver = crate::server::drivers()
        .get(ecosystem)
        .and_then(|driver| driver.capabilities().cache)
        .context(format!("the {ecosystem} ecosystem does not support cache purge"))?;
    let report = driver
        .purge_project(&stores.meta, &args.index, &args.project, args.yes)
        .map_err(anyhow::Error::msg)?;
    write_project_purge_report(out, if args.yes { "removed" } else { "dry-run" }, &args.index, &report)
}

pub(super) fn purge_orphaned_blobs(
    stores: &CacheStores,
    args: &CachePurgeOrphanedBlobsArgs,
    now: i64,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let referenced = referenced_blob_digests(&stores.meta)?;
    let candidates = orphan_candidates(&stores.blobs, &referenced)?;
    if !args.yes {
        // A dry run re-reads references after the walk and reports the survivors without touching a
        // byte, so a reference committed mid-scan spares its blob and the race that guards close never
        // arises.
        let referenced = referenced_blob_digests(&stores.meta)?;
        return report_orphans(&candidates, &referenced, out);
    }
    stores
        .meta
        .clear_blob_reclaim_guards()
        .context("clear stale orphan-deletion guards")?;
    let armed = arm_orphans(&stores.meta, &candidates, now, || referenced_blob_digests(&stores.meta))?;
    reclaim_guarded(&stores.blobs, &stores.meta, &candidates, &armed, out)
}

/// Arm a deletion guard for every candidate a fresh reference scan still leaves orphaned, returning the
/// indices armed. The scan and the arm run under a serial fence: a reference publication that raced the
/// scan advances the serial and fences the arm, so the scan is retaken until it and the guard write agree
/// on one store state. A guarded digest cannot then be referenced until its deletion finishes.
fn arm_orphans(
    meta: &MetaStore,
    candidates: &[OrphanCandidate],
    now: i64,
    mut scan: impl FnMut() -> anyhow::Result<BTreeSet<String>>,
) -> anyhow::Result<Vec<usize>> {
    loop {
        let serial = meta.current_serial().context("read the metadata serial")?;
        let referenced = scan()?;
        let still: Vec<usize> = candidates
            .iter()
            .enumerate()
            .filter(|(_, candidate)| !referenced.contains(candidate.digest.as_str()))
            .map(|(index, _)| index)
            .collect();
        let digests: Vec<&str> = still.iter().map(|&index| candidates[index].digest.as_str()).collect();
        if meta
            .arm_blob_reclaim_guards(&digests, serial, now)
            .context("arm orphan-deletion guards")?
        {
            return Ok(still);
        }
    }
}

/// Report and unlink each guarded candidate, disarming its guard once the bytes are gone. A guard held
/// the reference window shut from the fenced scan through the unlink, so a candidate reported here was
/// unreferenced across the whole deletion.
fn reclaim_guarded(
    blobs: &BlobStorage,
    meta: &MetaStore,
    candidates: &[OrphanCandidate],
    armed: &[usize],
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    writeln!(out, "action\ttarget\tdigest\tsize_bytes\tpath")?;
    let mut bytes = 0_u64;
    for &index in armed {
        let candidate = &candidates[index];
        bytes += candidate.bytes;
        blobs.blocking().delete(&candidate.digest)?;
        meta.disarm_blob_reclaim_guard(candidate.digest.as_str())
            .context("release orphan-deletion guard")?;
        let row = format!(
            "removed\torphaned-blob\t{}\t{}\t{}",
            candidate.digest.as_str(),
            candidate.bytes,
            candidate.path.display()
        );
        writeln!(out, "{row}")?;
    }
    writeln!(out, "summary\tremoved\torphaned-blobs\t{}\t{bytes}", armed.len())?;
    Ok(())
}

/// Report every candidate a fresh snapshot still does not name, without deleting anything. The dry-run
/// counterpart to [`reclaim_guarded`].
fn report_orphans(
    candidates: &[OrphanCandidate],
    referenced: &BTreeSet<String>,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    writeln!(out, "action\ttarget\tdigest\tsize_bytes\tpath")?;
    let mut count = 0_u64;
    let mut bytes = 0_u64;
    for candidate in candidates {
        if referenced.contains(candidate.digest.as_str()) {
            continue;
        }
        count += 1;
        bytes += candidate.bytes;
        writeln!(
            out,
            "dry-run\torphaned-blob\t{}\t{}\t{}",
            candidate.digest.as_str(),
            candidate.bytes,
            candidate.path.display()
        )?;
    }
    writeln!(out, "summary\tdry-run\torphaned-blobs\t{count}\t{bytes}")?;
    Ok(())
}

/// A blob on disk that the up-front reference snapshot did not name, and so a candidate for
/// collection pending a re-check against a fresh snapshot.
struct OrphanCandidate {
    digest: Digest,
    bytes: u64,
    path: PathBuf,
}

/// Walk the blob tree and gather every stored blob absent from `referenced`. Collecting the whole set
/// before reclaiming lets the caller re-read references once the walk is done, closing the window in
/// which a reference committed mid-scan would otherwise be missed.
fn orphan_candidates(blobs: &BlobStorage, referenced: &BTreeSet<String>) -> anyhow::Result<Vec<OrphanCandidate>> {
    let mut candidates = Vec::new();
    blobs
        .blocking()
        .visit(|entry| {
            if let Some(digest) = entry.digest
                && !referenced.contains(digest.as_str())
            {
                candidates.push(OrphanCandidate {
                    digest,
                    bytes: entry.bytes,
                    path: entry.path,
                });
            }
            Ok::<(), anyhow::Error>(())
        })
        .map_err(|err| anyhow::anyhow!("{err}"))
        .context("scan orphaned blob files")?;
    Ok(candidates)
}

/// Every blob digest any installed ecosystem's metadata references, unioned across drivers. Blobs are
/// content-addressed and shared, so a blob is orphaned only when no ecosystem names it; the collector
/// walks this whole set before reclaiming anything.
pub fn referenced_blob_digests(meta: &MetaStore) -> anyhow::Result<BTreeSet<String>> {
    let mut digests = BTreeSet::new();
    for serving in crate::server::drivers().present() {
        let Some(driver) = serving.capabilities().blob_references else {
            continue;
        };
        digests.extend(
            driver
                .referenced_blob_digests(meta)
                .map_err(|reason| anyhow::anyhow!("scan {} blob references: {reason}", serving.ecosystem().as_str()))?,
        );
    }
    Ok(digests)
}

fn write_project_purge_report(
    out: &mut dyn Write,
    action: &str,
    index: &str,
    report: &PurgeReport,
) -> anyhow::Result<()> {
    let mut header = "action\ttarget\tindex\tproject".to_owned();
    let mut row = format!("{action}\tproject\t{index}\t{}", report.project);
    for (category, count) in &report.categories {
        header.push('\t');
        header.push_str(category);
        row.push('\t');
        row.push_str(&count.to_string());
    }
    writeln!(out, "{header}")?;
    writeln!(out, "{row}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(dir: &std::path::Path) -> MetaStore {
        MetaStore::open(dir.join("peryx.redb")).unwrap()
    }

    #[test]
    fn test_arm_orphans_arms_every_unreferenced_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
        let orphan = blobs.blocking().put_bytes(b"orphan").unwrap();
        let meta = meta(dir.path());
        let candidates = orphan_candidates(&blobs, &BTreeSet::new()).unwrap();

        let armed = arm_orphans(&meta, &candidates, 7, || Ok(BTreeSet::new())).unwrap();

        assert_eq!(armed, vec![0]);
        assert!(meta.blob_reclaim_guarded(orphan.as_str()).unwrap());
    }

    #[test]
    fn test_arm_orphans_retries_when_a_reference_races_the_scan() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
        let orphan = blobs.blocking().put_bytes(b"orphan").unwrap();
        let raced = blobs.blocking().put_bytes(b"raced").unwrap();
        let meta = meta(dir.path());
        let candidates = orphan_candidates(&blobs, &BTreeSet::new()).unwrap();
        let raced_digest = raced.as_str().to_owned();

        let mut calls = 0;
        let armed = arm_orphans(&meta, &candidates, 0, || {
            calls += 1;
            if calls == 1 {
                // A writer publishes a reference to `raced`, advancing the serial and fencing the arm
                // that scanned before it. The retry rescans and spares the now-referenced blob.
                meta.commit_driver_txn(|txn| {
                    txn.reference_blob(&raced_digest, 5);
                    Ok::<_, peryx_storage::meta::MetaError>(((), vec![b"{}".to_vec()]))
                })
                .unwrap();
                Ok(BTreeSet::new())
            } else {
                Ok(BTreeSet::from([raced_digest.clone()]))
            }
        })
        .unwrap();

        assert_eq!(calls, 2, "the fenced first attempt forces one rescan");
        let armed_digests: Vec<&str> = armed.iter().map(|&index| candidates[index].digest.as_str()).collect();
        assert_eq!(armed_digests, vec![orphan.as_str()]);
        assert!(meta.blob_reclaim_guarded(orphan.as_str()).unwrap());
        assert!(!meta.blob_reclaim_guarded(raced.as_str()).unwrap());
    }

    #[test]
    fn test_reclaim_guarded_deletes_disarms_and_reports() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
        let orphan = blobs.blocking().put_bytes(b"orphan").unwrap();
        let meta = meta(dir.path());
        let candidates = orphan_candidates(&blobs, &BTreeSet::new()).unwrap();
        assert!(meta.arm_blob_reclaim_guards(&[orphan.as_str()], 0, 0).unwrap());

        let mut out = Vec::new();
        reclaim_guarded(&blobs, &meta, &candidates, &[0], &mut out).unwrap();

        assert!(blobs.blocking().head(&orphan).unwrap().is_none());
        assert!(!meta.blob_reclaim_guarded(orphan.as_str()).unwrap());
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains(&format!("removed\torphaned-blob\t{}\t6\t", orphan.as_str())));
        assert!(text.contains("summary\tremoved\torphaned-blobs\t1\t6\n"));
    }

    #[test]
    fn test_report_orphans_spares_a_referenced_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
        let orphan = blobs.blocking().put_bytes(b"orphan").unwrap();
        let kept = blobs.blocking().put_bytes(b"kept").unwrap();
        let candidates = orphan_candidates(&blobs, &BTreeSet::new()).unwrap();
        let referenced = BTreeSet::from([kept.as_str().to_owned()]);

        let mut out = Vec::new();
        report_orphans(&candidates, &referenced, &mut out).unwrap();

        let text = String::from_utf8(out).unwrap();
        assert!(text.contains(&format!("dry-run\torphaned-blob\t{}\t6\t", orphan.as_str())));
        assert!(!text.contains(kept.as_str()));
        assert!(text.contains("summary\tdry-run\torphaned-blobs\t1\t6\n"));
        assert!(blobs.blocking().head(&orphan).unwrap().is_some());
    }

    #[test]
    fn test_orphan_candidates_reports_a_scan_error() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("blobs");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("sha256"), b"not a directory").unwrap();
        let err = orphan_candidates(&BlobStorage::filesystem(&root), &BTreeSet::new())
            .err()
            .expect("scanning a corrupt store fails");
        assert!(err.to_string().contains("scan orphaned blob files"), "{err}");
    }
}
