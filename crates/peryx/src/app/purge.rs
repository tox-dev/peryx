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
        .context(format!("no driver for the {ecosystem} ecosystem"))?;
    let report = driver
        .purge_project(&stores.meta, &args.index, &args.project, args.yes)
        .map_err(anyhow::Error::msg)?;
    write_project_purge_report(out, if args.yes { "removed" } else { "dry-run" }, &args.index, &report)
}

pub(super) fn purge_orphaned_blobs(
    stores: &CacheStores,
    args: &CachePurgeOrphanedBlobsArgs,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let referenced = referenced_blob_digests(&stores.meta)?;
    let candidates = orphan_candidates(&stores.blobs, &referenced)?;
    // Re-read the reference set after the disk walk. An upload or mirror sync that committed a
    // reference to a blob already on disk while we scanned lands in this second snapshot but not the
    // first, so a blob it now names is spared rather than collected as an orphan.
    let referenced = referenced_blob_digests(&stores.meta)?;
    reclaim_orphans(&stores.blobs, args.yes, &candidates, &referenced, out)
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

/// Report and, under `yes`, unlink each candidate the fresh `referenced` snapshot still does not name.
/// A candidate a concurrent committer referenced during the walk shows up here and is left in place.
fn reclaim_orphans(
    blobs: &BlobStorage,
    yes: bool,
    candidates: &[OrphanCandidate],
    referenced: &BTreeSet<String>,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let action = if yes { "removed" } else { "dry-run" };
    writeln!(out, "action\ttarget\tdigest\tsize_bytes\tpath")?;
    let mut count = 0_u64;
    let mut bytes = 0_u64;
    for candidate in candidates {
        if referenced.contains(candidate.digest.as_str()) {
            continue;
        }
        count += 1;
        bytes += candidate.bytes;
        if yes {
            blobs.blocking().delete(&candidate.digest)?;
        }
        writeln!(
            out,
            "{action}\torphaned-blob\t{}\t{}\t{}",
            candidate.digest.as_str(),
            candidate.bytes,
            candidate.path.display()
        )?;
    }
    writeln!(out, "summary\t{action}\torphaned-blobs\t{count}\t{bytes}")?;
    Ok(())
}

/// Every blob digest any installed ecosystem's metadata references, unioned across drivers. Blobs are
/// content-addressed and shared, so a blob is orphaned only when no ecosystem names it; the collector
/// walks this whole set before reclaiming anything.
pub fn referenced_blob_digests(meta: &MetaStore) -> anyhow::Result<BTreeSet<String>> {
    let mut digests = BTreeSet::new();
    for driver in crate::server::drivers().present() {
        digests.extend(
            driver
                .referenced_blob_digests(meta)
                .map_err(|reason| anyhow::anyhow!("scan {} blob references: {reason}", driver.ecosystem().as_str()))?,
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

    #[test]
    fn test_reclaim_orphans_spares_a_reference_that_landed_during_the_walk() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
        let orphan = blobs.blocking().put_bytes(b"orphan").unwrap();
        let raced = blobs.blocking().put_bytes(b"raced").unwrap();
        let candidates = orphan_candidates(&blobs, &BTreeSet::new()).unwrap();
        // The committer named `raced` after the up-front snapshot; the fresh snapshot the walk hands
        // back now carries it, so only the true orphan is reclaimed.
        let referenced = BTreeSet::from([raced.as_str().to_owned()]);
        let mut out = Vec::new();
        reclaim_orphans(&blobs, true, &candidates, &referenced, &mut out).unwrap();
        assert!(blobs.blocking().head(&orphan).unwrap().is_none());
        assert!(blobs.blocking().head(&raced).unwrap().is_some());
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains(&format!("removed\torphaned-blob\t{}\t", orphan.as_str())));
        assert!(!text.contains(raced.as_str()));
        assert!(text.contains("summary\tremoved\torphaned-blobs\t1\t6\n"));
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
