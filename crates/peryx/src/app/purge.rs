//! Cache purging: per-project removal, orphaned-blob collection, and the reference scan.

use std::collections::BTreeSet;
use std::io::Write;

use anyhow::Context as _;
use peryx_driver::serving::PurgeReport;
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
        .with_context(|| format!("unknown index {:?}", args.index))?
        .ecosystem;
    let driver = crate::server::drivers()
        .get(ecosystem)
        .with_context(|| format!("no driver for the {ecosystem} ecosystem"))?;
    let report = driver
        .purge_project(&stores.meta, &args.index, &args.project, args.yes)
        .map_err(|reason| anyhow::anyhow!("{reason}"))?;
    write_project_purge_report(out, if args.yes { "removed" } else { "dry-run" }, &args.index, &report)
}

pub(super) fn purge_orphaned_blobs(
    stores: &CacheStores,
    args: &CachePurgeOrphanedBlobsArgs,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let references = referenced_blob_digests(&stores.meta)?;
    let mut count = 0_u64;
    let mut bytes = 0_u64;
    writeln!(out, "action\ttarget\tdigest\tsize_bytes\tpath")?;
    stores
        .blobs
        .scan(|entry| {
            let Some(digest) = &entry.digest else {
                return Ok::<(), anyhow::Error>(());
            };
            if references.contains(digest.as_str()) {
                return Ok(());
            }
            count += 1;
            bytes += entry.bytes;
            if args.yes {
                stores.blobs.remove(digest)?;
            }
            writeln!(
                out,
                "{}\torphaned-blob\t{}\t{}\t{}",
                if args.yes { "removed" } else { "dry-run" },
                digest.as_str(),
                entry.bytes,
                entry.path.display()
            )?;
            Ok(())
        })
        .map_err(|err| anyhow::anyhow!("{err}"))
        .context("scan orphaned blob files")?;
    writeln!(
        out,
        "summary\t{}\torphaned-blobs\t{count}\t{bytes}",
        if args.yes { "removed" } else { "dry-run" }
    )?;
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
