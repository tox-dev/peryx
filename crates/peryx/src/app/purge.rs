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
        if let Some(driver) = serving.capabilities().blob_references {
            digests.extend(driver.referenced_blob_digests(meta).map_err(anyhow::Error::msg)?);
        }
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
#[path = "../../tests/unit/app/purge/tests.rs"]
mod tests;
