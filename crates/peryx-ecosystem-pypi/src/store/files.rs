use std::collections::BTreeMap;

use peryx_ha::{ArtifactPlacement, ArtifactPlacementStore};
use peryx_storage::meta::{ArtifactOrigin, ArtifactSource, MetaError, MetaScanError, MetaStore};

use super::{
    FILE_PREFIX, METADATA_PREFIX, PROVENANCE_PREFIX, file_key, file_source_value, metadata_key, metadata_value,
    provenance_key, provenance_value,
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

/// Record where a blob digest can be fetched from: its upstream URL and the cached index it came from.
///
/// The index name lets a fetch on a cache miss reuse that index's authentication. Recording the locator
/// also registers the artifact's neutral placement. For a proxied artifact whose bytes are not yet local,
/// a later package read resolves availability from the index without probing the content store. A
/// re-discovery keeps a cached artifact's local state.
///
/// # Errors
/// Returns a store error if the write fails.
pub fn put_file_url(meta: &MetaStore, sha256: &str, url: &str, source: &str) -> Result<(), MetaError> {
    let value = file_source_value(url, source, None, None);
    meta.put_driver_value(&file_key(sha256), value.as_bytes())?;
    ArtifactPlacementStore::insert_artifact_placement(
        meta,
        sha256,
        &ArtifactPlacement::record(PypiArtifactOrigin::Cached.artifact_source(), false),
    )
    .map(|_| ())
}

/// # Errors
/// Returns a store error if the read fails.
pub fn get_file_url(meta: &MetaStore, sha256: &str) -> Result<Option<FileSource>, MetaError> {
    Ok(meta
        .get_driver_value(&file_key(sha256))?
        .and_then(|raw| String::from_utf8(raw).ok())
        .and_then(|value| split_file_source(&value)))
}

/// # Errors
/// Returns a scan error if the store read fails or the visitor returns an error.
pub fn scan_file_urls<E>(
    meta: &MetaStore,
    mut visit: impl FnMut(&str, &str) -> Result<(), E>,
) -> Result<(), MetaScanError<E>> {
    for key in meta.driver_prefix_keys(FILE_PREFIX)? {
        if let Some(value) = meta.get_driver_value(&key)?.and_then(|raw| String::from_utf8(raw).ok()) {
            visit(&key[FILE_PREFIX.len()..], &value).map_err(MetaScanError::Visit)?;
        }
    }
    Ok(())
}

/// Record the PEP 658 metadata sibling for an artifact: keyed by the artifact's digest,
/// storing the upstream `.metadata` URL and the metadata's own sha256 (for verify-on-fetch).
///
/// # Errors
/// Returns a store error if the write fails.
pub fn put_metadata(
    meta: &MetaStore,
    artifact_sha256: &str,
    url: &str,
    metadata_sha256: &str,
    source: &str,
) -> Result<(), MetaError> {
    let value = metadata_value(url, metadata_sha256, source);
    meta.put_driver_value(&metadata_key(artifact_sha256), value.as_bytes())
}

/// # Errors
/// Returns a store error if the read fails.
pub fn get_metadata(meta: &MetaStore, artifact_sha256: &str) -> Result<Option<(String, String, String)>, MetaError> {
    Ok(meta
        .get_driver_value(&metadata_key(artifact_sha256))?
        .and_then(|raw| String::from_utf8(raw).ok())
        .and_then(|value| {
            let mut parts = value.splitn(3, '\n');
            Some((
                parts.next()?.to_owned(),
                parts.next()?.to_owned(),
                parts.next()?.to_owned(),
            ))
        }))
}

/// # Errors
/// Returns a store error if the read fails.
pub fn get_metadata_digests<'a>(
    meta: &MetaStore,
    artifact_sha256s: impl IntoIterator<Item = &'a str>,
) -> Result<BTreeMap<String, String>, MetaError> {
    let mut metadata = BTreeMap::new();
    for artifact_sha256 in artifact_sha256s {
        let Some(value) = meta
            .get_driver_value(&metadata_key(artifact_sha256))?
            .and_then(|raw| String::from_utf8(raw).ok())
        else {
            continue;
        };
        let mut parts = value.splitn(3, '\n');
        let (_url, Some(metadata_sha256), _source) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        metadata.insert(artifact_sha256.to_owned(), metadata_sha256.to_owned());
    }
    Ok(metadata)
}

/// Visit raw PEP 658 metadata records, keyed by wheel digest.
///
/// # Errors
/// Returns a scan error if the store read fails or the visitor returns an error.
pub fn scan_metadata_records<E>(
    meta: &MetaStore,
    mut visit: impl FnMut(&str, &str) -> Result<(), E>,
) -> Result<(), MetaScanError<E>> {
    for key in meta.driver_prefix_keys(METADATA_PREFIX)? {
        if let Some(value) = meta.get_driver_value(&key)?.and_then(|raw| String::from_utf8(raw).ok()) {
            visit(&key[METADATA_PREFIX.len()..], &value).map_err(MetaScanError::Visit)?;
        }
    }
    Ok(())
}

/// Record a distribution's PEP 740 provenance sibling: keyed by the artifact's digest, storing the
/// provenance blob's own sha256 and its byte length.
///
/// # Errors
/// Returns a store error if the write fails.
pub fn put_provenance(
    meta: &MetaStore,
    artifact_sha256: &str,
    provenance_sha256: &str,
    size: u64,
) -> Result<(), MetaError> {
    meta.put_driver_value(
        &provenance_key(artifact_sha256),
        provenance_value(provenance_sha256, size).as_bytes(),
    )
}

/// # Errors
/// Returns a store error if the read fails.
pub fn get_provenance(meta: &MetaStore, artifact_sha256: &str) -> Result<Option<(String, u64)>, MetaError> {
    Ok(meta
        .get_driver_value(&provenance_key(artifact_sha256))?
        .and_then(|raw| String::from_utf8(raw).ok())
        .and_then(|value| split_provenance(&value)))
}

/// # Errors
/// Returns a scan error if the store read fails or the visitor returns an error.
pub fn scan_provenance_records<E>(
    meta: &MetaStore,
    mut visit: impl FnMut(&str, &str) -> Result<(), E>,
) -> Result<(), MetaScanError<E>> {
    for key in meta.driver_prefix_keys(PROVENANCE_PREFIX)? {
        if let Some(value) = meta.get_driver_value(&key)?.and_then(|raw| String::from_utf8(raw).ok()) {
            visit(&key[PROVENANCE_PREFIX.len()..], &value).map_err(MetaScanError::Visit)?;
        }
    }
    Ok(())
}

/// Split a provenance value into `(provenance sha256, byte length)`, rejecting a record missing
/// either field.
fn split_provenance(value: &str) -> Option<(String, u64)> {
    let (sha256, size) = value.split_once('\n')?;
    Some((sha256.to_owned(), size.parse().ok()?))
}

fn split_file_source(value: &str) -> Option<FileSource> {
    let mut parts = value.splitn(4, '\n');
    Some(FileSource {
        url: parts.next()?.to_owned(),
        source: parts.next()?.to_owned(),
        size: parts.next().and_then(|size| size.parse().ok()),
        upstream: parts.next().filter(|upstream| !upstream.is_empty()).map(str::to_owned),
    })
}

#[cfg(test)]
#[path = "../../tests/unit/store/files/tests.rs"]
mod tests;
