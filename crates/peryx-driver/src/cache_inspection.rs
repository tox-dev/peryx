//! Rendering for the cache reports.
//!
//! The offline `peryx cache` commands and the admin endpoints that answer the same questions while
//! the server holds the store differ only in where the rows come from. Keeping the writers here is
//! what lets the online numbers be the offline numbers rather than a second implementation of them.

use std::collections::BTreeSet;
use std::io::Write;

use peryx_storage::blob::{BlobEntry, BlobError, BlobScanError, BlobStorage};
use peryx_storage::meta::{MetaError, MetaStore};

use crate::DriverSet;
use crate::serving::{CachePage, NameDriver};

pub struct CacheListFilter<'a> {
    pub index: Option<&'a str>,
    pub resource_filtered: bool,
    pub digest: Option<&'a str>,
    pub stale: bool,
    pub min_age_secs: Option<u64>,
    pub min_size_bytes: Option<u64>,
}

pub struct CachePageSource {
    pub pages: Vec<CachePage>,
    pub resource_filter: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum CacheInspectionError {
    #[error("{0}")]
    Write(#[source] std::io::Error),
    #[error("scan cached index pages")]
    PageOutput(#[source] std::io::Error),
    #[error("scan blob files")]
    BlobScan(#[source] BlobScanError<std::io::Error>),
    #[error("scan blob stages")]
    BlobStages(#[source] BlobError),
    #[error("read repository ecosystems")]
    RepositoryEcosystems(#[source] MetaError),
    #[error("fsck ecosystem metadata: {0}")]
    EcosystemFsck(String),
    #[error("repair ecosystem metadata: {0}")]
    EcosystemRepair(String),
}

/// Normalizes a resource filter the way the stored keys it is compared against were normalized.
///
/// An ecosystem without a name driver defines no normalization, so its keys are matched verbatim.
#[must_use]
pub fn resource_filter(resource: Option<&str>, names: Option<&dyn NameDriver>) -> Option<String> {
    match (resource, names) {
        (Some(resource), Some(driver)) => Some(driver.normalize_name(resource)),
        (Some(resource), None) => Some(resource.to_owned()),
        (None, _) => None,
    }
}

/// # Errors
/// Returns an output or blob scan error.
pub fn write_cache_list(
    sources: Vec<CachePageSource>,
    blobs: &BlobStorage,
    filter: &CacheListFilter<'_>,
    ttl_secs: i64,
    now: i64,
    out: &mut dyn Write,
) -> Result<(), CacheInspectionError> {
    writeln!(
        out,
        "kind\tindex\tresource\tdigest\tage_secs\tfresh_secs\tstale\tsize_bytes\tkey"
    )
    .map_err(CacheInspectionError::Write)?;
    for source in sources {
        for page in source.pages {
            let age = age_secs(now, page.fetched_at_unix);
            let stale = is_stale(age, page.fresh_secs.unwrap_or(ttl_secs));
            if filter.index.is_some_and(|index| index != page.index)
                || source
                    .resource_filter
                    .as_deref()
                    .is_some_and(|resource| resource != page.resource)
                || filter.stale && !stale
                || filter.min_age_secs.is_some_and(|min| age < min)
                || filter.min_size_bytes.is_some_and(|min| page.body_bytes < min)
            {
                continue;
            }
            writeln!(
                out,
                "index\t{}\t{}\t\t{age}\t{}\t{stale}\t{}\t{}",
                page.index,
                page.resource,
                page.fresh_secs.map_or_else(|| "-".to_owned(), |secs| secs.to_string()),
                page.body_bytes,
                page.key,
            )
            .map_err(CacheInspectionError::PageOutput)?;
        }
    }
    if filter.index.is_some() || filter.resource_filtered || filter.stale || filter.min_age_secs.is_some() {
        return Ok(());
    }
    blobs
        .blocking()
        .visit(|entry| write_blob_entry(out, filter, &entry))
        .map_err(CacheInspectionError::BlobScan)
}

/// # Errors
/// Returns an output, blob scan, or stage scan error.
pub fn write_cache_size(
    pages: &[CachePage],
    record_counts: Vec<(String, u64)>,
    blobs: &BlobStorage,
    ttl_secs: i64,
    now: i64,
    out: &mut dyn Write,
) -> Result<(), CacheInspectionError> {
    let mut index_bytes = 0_u64;
    let mut stale_index_pages = 0_u64;
    for page in pages {
        index_bytes += page.record_bytes;
        stale_index_pages += u64::from(is_stale(
            age_secs(now, page.fetched_at_unix),
            page.fresh_secs.unwrap_or(ttl_secs),
        ));
    }
    let mut blob_files = 0_u64;
    let mut blob_bytes = 0_u64;
    let mut invalid_blob_paths = 0_u64;
    blobs
        .blocking()
        .visit(|entry| {
            blob_files += 1;
            blob_bytes += entry.bytes;
            invalid_blob_paths += u64::from(entry.digest.is_none());
            Ok::<(), std::io::Error>(())
        })
        .map_err(CacheInspectionError::BlobScan)?;
    let stages = blobs
        .blocking()
        .stage_usage()
        .map_err(CacheInspectionError::BlobStages)?;
    writeln!(out, "index_pages\t{}", pages.len()).map_err(CacheInspectionError::Write)?;
    writeln!(out, "stale_index_pages\t{stale_index_pages}").map_err(CacheInspectionError::Write)?;
    writeln!(out, "index_bytes\t{index_bytes}").map_err(CacheInspectionError::Write)?;
    writeln!(out, "blob_files\t{blob_files}").map_err(CacheInspectionError::Write)?;
    writeln!(out, "blob_bytes\t{blob_bytes}").map_err(CacheInspectionError::Write)?;
    writeln!(out, "invalid_blob_paths\t{invalid_blob_paths}").map_err(CacheInspectionError::Write)?;
    writeln!(out, "stage_files\t{}", stages.files).map_err(CacheInspectionError::Write)?;
    writeln!(out, "stage_bytes\t{}", stages.bytes).map_err(CacheInspectionError::Write)?;
    for (label, count) in record_counts {
        writeln!(out, "{label}\t{count}").map_err(CacheInspectionError::Write)?;
    }
    Ok(())
}

/// # Errors
/// Returns a metadata, ecosystem, blob scan, or output error.
pub fn write_cache_fsck(
    drivers: &DriverSet,
    meta: &MetaStore,
    blobs: &BlobStorage,
    indexes: &[peryx_index::Index],
    out: &mut dyn Write,
) -> Result<(), CacheInspectionError> {
    let mut problems = 0_u64;
    let mut ecosystem_drivers = drivers.fsck_drivers().collect::<Vec<_>>();
    ecosystem_drivers.sort_unstable_by_key(|(ecosystem, _)| ecosystem.as_str());
    let checked = ecosystem_drivers
        .iter()
        .map(|(ecosystem, _)| ecosystem.as_str())
        .collect::<BTreeSet<_>>();
    for ecosystem in meta
        .repository_ecosystems()
        .map_err(CacheInspectionError::RepositoryEcosystems)?
        .iter()
        .filter(|ecosystem| !checked.contains(ecosystem.as_str()))
    {
        writeln!(out, "metadata\t{ecosystem}\tmissing checker").map_err(CacheInspectionError::Write)?;
        problems += 1;
    }
    for (_, driver) in ecosystem_drivers {
        problems += driver
            .fsck_metadata(meta, blobs, indexes, out)
            .map_err(CacheInspectionError::EcosystemFsck)?;
    }
    blobs
        .blocking()
        .visit(|entry| {
            problems += check_blob(blobs, &entry, out)?;
            Ok::<(), std::io::Error>(())
        })
        .map_err(CacheInspectionError::BlobScan)?;
    if problems == 0 {
        writeln!(out, "ok").map_err(CacheInspectionError::Write)?;
    } else {
        writeln!(out, "problems\t{problems}").map_err(CacheInspectionError::Write)?;
    }
    Ok(())
}

/// Rebuild the derived records `fsck` reports, or, when `apply` is false, report what a rebuild would
/// write without writing it.
///
/// A preview and a rebuild name the same records, because both come from the same comparison against
/// the records those derived rows summarize. Only the rebuild needs a writable store, which is why the
/// preview stays available while the server holds one.
///
/// # Errors
/// Returns an ecosystem repair or output error.
pub fn write_cache_repair(
    drivers: &DriverSet,
    meta: &MetaStore,
    indexes: &[peryx_index::Index],
    apply: bool,
    out: &mut dyn Write,
) -> Result<(), CacheInspectionError> {
    let mut ecosystem_drivers = drivers.metadata_repair_drivers().collect::<Vec<_>>();
    ecosystem_drivers.sort_unstable_by_key(|(ecosystem, _)| ecosystem.as_str());
    let mut repaired = 0_u64;
    for (_, driver) in ecosystem_drivers {
        repaired += if apply {
            driver.repair_metadata(meta, indexes, out)
        } else {
            driver.preview_metadata_repair(meta, indexes, out)
        }
        .map_err(CacheInspectionError::EcosystemRepair)?;
    }
    let label = if apply { "repaired" } else { "planned" };
    writeln!(out, "{label}\t{repaired}").map_err(CacheInspectionError::Write)?;
    Ok(())
}

fn write_blob_entry(out: &mut dyn Write, filter: &CacheListFilter<'_>, entry: &BlobEntry) -> std::io::Result<()> {
    let Some(digest) = &entry.digest else {
        return Ok(());
    };
    if filter.digest.is_some_and(|value| value != digest.as_str())
        || filter.min_size_bytes.is_some_and(|min| entry.bytes < min)
    {
        return Ok(());
    }
    writeln!(
        out,
        "blob\t\t\t{}\t-\t-\t-\t{}\t{}",
        digest.as_str(),
        entry.bytes,
        entry.path.display()
    )
}

fn check_blob(blobs: &BlobStorage, entry: &BlobEntry, out: &mut dyn Write) -> std::io::Result<u64> {
    let Some(digest) = &entry.digest else {
        writeln!(
            out,
            "blob\tpath\t{}\tinvalid content-addressed path",
            entry.path.display()
        )?;
        return Ok(1);
    };
    match blobs.blocking().verify(digest) {
        Ok(true) => Ok(0),
        Ok(false) => {
            writeln!(out, "blob\thash\t{}\tdigest mismatch", digest.as_str())?;
            Ok(1)
        }
        Err(error) => {
            writeln!(out, "blob\tread\t{}\t{error}", digest.as_str())?;
            Ok(1)
        }
    }
}

fn age_secs(now: i64, fetched_at: i64) -> u64 {
    now.saturating_sub(fetched_at).try_into().unwrap_or_default()
}

const fn is_stale(age: u64, ttl: i64) -> bool {
    ttl <= 0 || age >= ttl.cast_unsigned()
}
