use std::io::Write;

use anyhow::Context as _;
use peryx_driver::DriverSet;
use peryx_driver::serving::PurgeReport;
use peryx_ha::AvailabilityMode;
use peryx_ha_distributed::OrphanPurgeReport;

use super::CacheStores;
use crate::cli::{CachePurgeOrphanedBlobsArgs, CachePurgeResourceArgs};
use crate::config::Config;

pub(super) fn purge_resource(
    config: &Config,
    drivers: &DriverSet,
    stores: &CacheStores,
    args: &CachePurgeResourceArgs,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let ecosystem = config
        .indexes
        .iter()
        .find(|index| index.name == args.index)
        .context(format!("unknown index {:?}", args.index))?
        .ecosystem
        .clone();
    let driver = drivers
        .get_cache(&ecosystem)
        .context(format!("the {ecosystem} ecosystem does not support cache purge"))?;
    let report = driver
        .purge_resource(&stores.meta, &args.index, &args.resource, args.yes)
        .map_err(anyhow::Error::msg)?;
    write_resource_purge_report(out, if args.yes { "removed" } else { "dry-run" }, &args.index, &report)
}

pub(super) fn purge_orphaned_blobs(
    drivers: &DriverSet,
    stores: &CacheStores,
    args: &CachePurgeOrphanedBlobsArgs,
    now: i64,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let mut ecosystems = None;
    let report = peryx_ha_distributed::purge_orphaned_blobs(&stores.meta, &stores.blobs, args.yes, now, || {
        let scan = drivers
            .scan_blob_references(&stores.meta)
            .map_err(|error| error.to_string())?;
        ecosystems.get_or_insert(scan.ecosystems);
        Ok(scan.digests)
    })?;
    write_orphan_purge_report(
        out,
        if args.yes { "removed" } else { "dry-run" },
        ecosystems.as_ref().expect("a successful purge scans references"),
        &report,
    )
}

fn write_orphan_purge_report(
    out: &mut dyn Write,
    action: &str,
    ecosystems: &[String],
    report: &OrphanPurgeReport,
) -> anyhow::Result<()> {
    writeln!(out, "action\ttarget\tdigest\tsize_bytes\tpath")?;
    for blob in &report.blobs {
        writeln!(
            out,
            "{action}\torphaned-blob\t{}\t{}\t{}",
            blob.digest,
            blob.bytes,
            blob.path.display()
        )?;
    }
    let blobs = report.blobs.len();
    writeln!(out, "summary\t{action}\torphaned-blobs\t{blobs}\t{}", report.bytes)?;
    writeln!(out, "scope\tecosystems\t{}", ecosystems.join(","))?;
    Ok(())
}

pub(super) fn validate_orphan_purge_mode(mode: AvailabilityMode) -> anyhow::Result<()> {
    anyhow::ensure!(
        mode == AvailabilityMode::None,
        "orphaned blob purge is unsupported while availability mode {mode:?} is configured"
    );
    Ok(())
}

fn write_resource_purge_report(
    out: &mut dyn Write,
    action: &str,
    index: &str,
    report: &PurgeReport,
) -> anyhow::Result<()> {
    let mut header = "action\ttarget\tindex\tresource".to_owned();
    let mut row = format!("{action}\tresource\t{index}\t{}", report.resource);
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

#[cfg(test)]
#[path = "../../tests/unit/tests/app/purge_tests.rs"]
mod external_tests;
