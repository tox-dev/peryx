use std::collections::BTreeMap;

use peryx_ha::{ArtifactPlacement, ArtifactPlacementStore};
use peryx_storage::meta::{ArtifactOrigin, ArtifactSource, MetaError, MetaScanError, MetaStore};

use super::{
    FILE_PREFIX, METADATA_PREFIX, PROVENANCE_PREFIX, PUBLICATION_PREFIX, ProvenanceSibling, file_key,
    file_source_value, metadata_key, provenance_key, provenance_value, publication_key, record_str, scan_utf8_records,
    split_provenance_value,
};

/// Where a `PyPI` artifact's bytes came from, mapped once into the neutral [`ArtifactSource`] so no
/// neutral code decides a `PyPI` file's origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PypiArtifactOrigin {
    /// Uploaded into a hosted index on this instance.
    Upload,
    /// Cached from an upstream Simple index.
    Cached,
}

impl ArtifactOrigin for PypiArtifactOrigin {
    fn artifact_source(&self) -> ArtifactSource {
        match self {
            Self::Upload => ArtifactSource::Hosted,
            Self::Cached => ArtifactSource::Proxy,
        }
    }
}

/// The upstream source for a cached artifact digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSource {
    pub url: String,
    pub source: String,
    pub size: Option<u64>,
    /// The named routed upstream that advertised this artifact.
    pub upstream: Option<String>,
}

/// Record where one index's publication of a blob digest can be fetched from: its upstream URL and the
/// cached index it came from.
///
/// The index name lets a fetch on a cache miss reuse that index's authentication, which is why the row
/// belongs to the publication rather than to the digest: two indexes may both advertise one artifact,
/// and each is right about a different upstream and a different credential. Recording the locator also
/// registers the artifact's neutral placement. For a proxied artifact whose bytes are not yet local, a
/// later package read resolves availability from the index without probing the content store. A
/// re-discovery keeps a cached artifact's local state.
///
/// # Errors
/// Returns a store error if the write fails.
pub fn put_file_url(
    meta: &MetaStore,
    index: &str,
    normalized: &str,
    sha256: &str,
    url: &str,
    source: &str,
) -> Result<(), MetaError> {
    let value = file_source_value(url, source, None, None);
    meta.put_driver_value(&file_key(index, normalized, sha256), value.as_bytes())?;
    ArtifactPlacementStore::insert_artifact_placement(
        meta,
        sha256,
        &ArtifactPlacement::record(PypiArtifactOrigin::Cached.artifact_source(), false),
    )
    .map(|_| ())
}

/// # Errors
/// Returns a store error if the read fails or the source record is invalid.
pub fn get_file_url(
    meta: &MetaStore,
    index: &str,
    normalized: &str,
    sha256: &str,
) -> Result<Option<FileSource>, MetaError> {
    let key = file_key(index, normalized, sha256);
    meta.get_driver_value(&key)?
        .map(|raw| record_str(&key, raw).and_then(|value| split_file_source(&key, &value)))
        .transpose()
}

/// # Errors
/// Returns a scan error if the store read fails or the visitor returns an error.
/// Visit every source row as `(index, normalized, sha256, value)`.
///
/// The key names the publication that advertised the download, so its last segment is the digest and
/// the rest is the owner. Splitting here rather than at each caller is what keeps a reader from
/// handing `{index}/{normalized}/{sha256}` to something that expects a digest: the orphan-blob
/// collector reads this scan, and a whole key mistaken for a digest would spare no blob at all.
///
/// # Errors
/// Returns a scan error if the store read fails, a key does not carry all three parts, or the
/// visitor returns an error.
pub fn scan_file_urls<E>(
    meta: &MetaStore,
    mut visit: impl FnMut(&str, &str, &str, &str) -> Result<(), E>,
) -> Result<(), MetaScanError<E>> {
    meta.scan_driver_prefix(FILE_PREFIX, |key, raw| {
        let Some((index, normalized, sha256)) = split_file_source_key(&key[FILE_PREFIX.len()..]) else {
            return Ok(());
        };
        let value = record_str(key, raw.to_vec())?;
        visit(index, normalized, sha256, &value).map_err(MetaScanError::Visit)
    })
}

/// The `{index}/{normalized}/{sha256}` a source key carries, or `None` for a row written before the
/// key named an owner.
///
/// One definition of the shape, shared by every reader: the scan that feeds the orphan-blob collector
/// and the `fsck` predicate both need the digest out of the key, and each parsing it for itself is how
/// one of them ends up reading the whole key as a digest.
pub fn split_file_source_key(key: &str) -> Option<(&str, &str, &str)> {
    let (index, rest) = key.split_once('/')?;
    let (normalized, sha256) = rest.split_once('/')?;
    (!index.is_empty() && !normalized.is_empty() && !sha256.is_empty()).then_some((index, normalized, sha256))
}

/// Delete every source row written before the key named an owner, reporting how many went.
///
/// Such a row records a URL, an index and an upstream but not which publication they belonged to, and
/// peryx cannot recover that: the whole defect was that the last writer's answer overwrote everyone
/// else's. Inventing an owner would re-assert exactly the claim that was wrong, so the row is dropped
/// and the next page fetch or catalog sync writes an owned one in its place. Until then the artifact
/// has no cached locator, so a cold download for it fetches the page again first, and a blob nothing
/// else references may be reclaimed before that happens and downloaded again after.
///
/// # Errors
/// Returns a store error if the scan or the removal fails.
pub fn drop_legacy_file_sources(meta: &MetaStore) -> Result<usize, MetaError> {
    let mut legacy = Vec::new();
    meta.scan_driver_prefix(FILE_PREFIX, |key, _value| {
        if split_file_source_key(&key[FILE_PREFIX.len()..]).is_none() {
            legacy.push(key.to_owned());
        }
        Ok::<(), MetaError>(())
    })?;
    meta.commit_driver_txn(|txn| {
        let mut dropped = 0;
        for key in &legacy {
            dropped += usize::from(txn.remove_local(key)?);
        }
        Ok((dropped, Vec::new()))
    })
}

/// Record the metadata peryx derived from an artifact's own verified bytes, keyed by that digest.
///
/// Extraction and upload both produce the same bytes for the same digest, so the record is a
/// property of the artifact and every publication of it may serve this.
///
/// A sidecar an upstream index merely *claims* to hold does not belong here; it is scoped to the
/// publication that advertised it, through [`get_file_publication`].
///
/// # Errors
/// Returns a store error if the write fails.
pub fn put_metadata(meta: &MetaStore, artifact_sha256: &str, metadata_sha256: &str) -> Result<(), MetaError> {
    meta.put_driver_value(&metadata_key(artifact_sha256), metadata_sha256.as_bytes())
}

/// # Errors
/// Returns a store error if the read fails or the record is not valid UTF-8.
pub fn get_metadata_digest(meta: &MetaStore, artifact_sha256: &str) -> Result<Option<String>, MetaError> {
    let key = metadata_key(artifact_sha256);
    meta.get_driver_value(&key)?
        .map(|raw| record_str(&key, raw))
        .transpose()
}

/// # Errors
/// Returns a store error if the read fails.
pub fn get_metadata_digests<'a>(
    meta: &MetaStore,
    artifact_sha256s: impl IntoIterator<Item = &'a str>,
) -> Result<BTreeMap<String, String>, MetaError> {
    let mut metadata = BTreeMap::new();
    for artifact_sha256 in artifact_sha256s {
        if let Some(metadata_sha256) = get_metadata_digest(meta, artifact_sha256)? {
            metadata.insert(artifact_sha256.to_owned(), metadata_sha256);
        }
    }
    Ok(metadata)
}

/// The PEP 658 sidecar one cached index advertised for one file, with the credentials that reach it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataClaim {
    /// The sidecar URL the publishing index advertised.
    pub url: String,
    /// The sidecar's own sha256, which the page advertises and an installer verifies.
    pub metadata_sha256: String,
    /// The configured index whose credentials reach `url`.
    pub source: String,
    /// The named routed upstream that advertised the file, when the source routes to several.
    pub upstream: Option<String>,
}

/// What one index's page said about a file's PEP 658 sidecar.
///
/// A resolver walks a virtual index's layers in shadow order and stops at the first layer that
/// published the file. [`Self::Unclaimed`] is what makes that stop meaningful: the winning layer
/// listed the file and advertised no sidecar, so a shadowed layer's claim must not be inherited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilePublication {
    /// The page advertised a sidecar.
    Claimed(MetadataClaim),
    /// The page listed the file with no sidecar.
    Unclaimed,
}

/// # Errors
/// Returns a store error if the read fails or the record is not valid UTF-8 or is truncated.
pub fn get_file_publication(
    meta: &MetaStore,
    index: &str,
    normalized: &str,
    sha256: &str,
    filename: &str,
) -> Result<Option<FilePublication>, MetaError> {
    let key = publication_key(index, normalized, sha256, filename);
    meta.get_driver_value(&key)?
        .map(|raw| record_str(&key, raw).and_then(|value| split_publication(&key, &value)))
        .transpose()
}

/// # Errors
/// Returns a scan error if the store read fails or the visitor returns an error.
pub fn scan_file_publications<E>(
    meta: &MetaStore,
    visit: impl FnMut(&str, &str) -> Result<(), E>,
) -> Result<(), MetaScanError<E>> {
    scan_utf8_records(meta, PUBLICATION_PREFIX, visit)
}

fn split_publication(key: &str, value: &str) -> Result<FilePublication, MetaError> {
    if value.is_empty() {
        return Ok(FilePublication::Unclaimed);
    }
    let mut parts = value.splitn(4, '\n');
    let missing = |field| MetaError::DriverRecordMissing {
        key: key.to_owned(),
        field,
    };
    let url = parts.next().unwrap_or_default();
    let metadata_sha256 = parts.next().ok_or_else(|| missing("metadata_sha256"))?;
    let source = parts.next().ok_or_else(|| missing("source"))?;
    let upstream = parts.next().ok_or_else(|| missing("upstream"))?;
    Ok(FilePublication::Claimed(MetadataClaim {
        url: url.to_owned(),
        metadata_sha256: metadata_sha256.to_owned(),
        source: source.to_owned(),
        upstream: (!upstream.is_empty()).then(|| upstream.to_owned()),
    }))
}

/// Visit raw PEP 658 metadata records, keyed by wheel digest.
///
/// # Errors
/// Returns a scan error if the store read fails or the visitor returns an error.
pub fn scan_metadata_records<E>(
    meta: &MetaStore,
    visit: impl FnMut(&str, &str) -> Result<(), E>,
) -> Result<(), MetaScanError<E>> {
    scan_utf8_records(meta, METADATA_PREFIX, visit)
}

/// Record one hosted publication's PEP 740 provenance bundle, storing the bundle blob's own sha256
/// and its byte length.
///
/// The bundle is scoped to the publication rather than to the artifact digest because it is what one
/// publisher attested about its own release: two hosted indexes may accept different bundles for
/// byte-identical distributions, and neither may serve the other's.
///
/// # Errors
/// Returns a store error if the write fails.
pub fn put_provenance(
    meta: &MetaStore,
    index: &str,
    normalized: &str,
    artifact_sha256: &str,
    filename: &str,
    bundle: ProvenanceSibling<'_>,
) -> Result<(), MetaError> {
    meta.put_driver_value(
        &provenance_key(index, normalized, artifact_sha256, filename),
        provenance_value(bundle.provenance_sha256, bundle.size).as_bytes(),
    )
}

/// # Errors
/// Returns a store error if the read fails or the bundle record is invalid.
pub fn get_provenance(
    meta: &MetaStore,
    index: &str,
    normalized: &str,
    artifact_sha256: &str,
    filename: &str,
) -> Result<Option<(String, u64)>, MetaError> {
    let key = provenance_key(index, normalized, artifact_sha256, filename);
    meta.get_driver_value(&key)?
        .map(|raw| {
            let value = record_str(&key, raw)?;
            split_provenance_value(&key, &value).map(|(sha256, size)| (sha256.to_owned(), size))
        })
        .transpose()
}

/// # Errors
/// Returns a scan error if the store read fails or the visitor returns an error.
pub fn scan_provenance_records<E>(
    meta: &MetaStore,
    visit: impl FnMut(&str, &str) -> Result<(), E>,
) -> Result<(), MetaScanError<E>> {
    scan_utf8_records(meta, PROVENANCE_PREFIX, visit)
}

fn split_file_source(key: &str, value: &str) -> Result<FileSource, MetaError> {
    let mut parts = value.splitn(4, '\n');
    let url = parts.next().unwrap_or_default();
    let source = parts.next().ok_or_else(|| MetaError::DriverRecordMissing {
        key: key.to_owned(),
        field: "source",
    })?;
    let size = parts
        .next()
        .filter(|size| !size.is_empty())
        .map(str::parse)
        .transpose()
        .map_err(|source| MetaError::DriverRecordInteger {
            key: key.to_owned(),
            field: "size",
            source,
        })?;
    Ok(FileSource {
        url: url.to_owned(),
        source: source.to_owned(),
        size,
        upstream: parts.next().filter(|upstream| !upstream.is_empty()).map(str::to_owned),
    })
}

#[cfg(test)]
#[path = "../../tests/unit/store/files/tests.rs"]
mod tests;
