use std::collections::BTreeSet;
use std::io::Write;

use anyhow::Context as _;
use peryx_driver::DriverSet;
use peryx_storage::blob::BlobEntry;

use super::CacheStores;

pub(super) fn fsck_cache(drivers: &DriverSet, stores: &CacheStores, out: &mut dyn Write) -> anyhow::Result<()> {
    let mut problems = 0_u64;
    let mut ecosystem_drivers = drivers.fsck_drivers().collect::<Vec<_>>();
    ecosystem_drivers.sort_unstable_by_key(|(ecosystem, _)| ecosystem.as_str());
    let checked = ecosystem_drivers
        .iter()
        .map(|(ecosystem, _)| ecosystem.as_str())
        .collect::<BTreeSet<_>>();
    for ecosystem in stores
        .meta
        .repository_ecosystems()
        .context("read repository ecosystems")?
        .iter()
        .filter(|ecosystem| !checked.contains(ecosystem.as_str()))
    {
        writeln!(out, "metadata\t{ecosystem}\tmissing checker")?;
        problems += 1;
    }
    for (_, driver) in ecosystem_drivers {
        problems += driver
            .fsck_metadata(&stores.meta, &stores.blobs, out)
            .map_err(anyhow::Error::msg)
            .context("fsck ecosystem metadata")?;
    }
    stores
        .blobs
        .blocking()
        .visit(|entry| {
            problems += check_blob(stores, &entry, out)?;
            Ok::<(), std::io::Error>(())
        })
        .context("scan blob files")?;
    if problems == 0 {
        writeln!(out, "ok")?;
    } else {
        writeln!(out, "problems\t{problems}")?;
    }
    Ok(())
}

fn check_blob(stores: &CacheStores, entry: &BlobEntry, out: &mut dyn Write) -> std::io::Result<u64> {
    let Some(digest) = &entry.digest else {
        writeln!(
            out,
            "blob\tpath\t{}\tinvalid content-addressed path",
            entry.path.display()
        )?;
        return Ok(1);
    };
    match stores.blobs.blocking().verify(digest) {
        Ok(true) => Ok(0),
        Ok(false) => {
            writeln!(out, "blob\thash\t{}\tdigest mismatch", digest.as_str())?;
            Ok(1)
        }
        Err(err) => {
            writeln!(out, "blob\tread\t{}\t{err}", digest.as_str())?;
            Ok(1)
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/tests/app/fsck_tests.rs"]
mod tests;
