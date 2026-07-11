//! Cache consistency checks: each ecosystem's metadata records, then the content-addressed blobs.

use std::io::Write;

use anyhow::Context as _;
use peryx_storage::blob::BlobEntry;

use super::CacheStores;

pub(super) fn fsck_cache(stores: &CacheStores, out: &mut dyn Write) -> anyhow::Result<()> {
    let mut problems = 0_u64;
    for driver in crate::server::drivers().present() {
        problems += driver
            .fsck_metadata(&stores.meta, &stores.blobs, out)
            .map_err(anyhow::Error::msg)
            .context(format!("fsck {} metadata", driver.ecosystem().as_str()))?;
    }
    stores
        .blobs
        .scan(|entry| {
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
    match stores.blobs.verify(digest) {
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
