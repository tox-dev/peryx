use std::collections::HashMap;

use peryx_storage::meta::{MetaError, MetaScanError, MetaStore};

use super::{FILE_PREFIX, METADATA_PREFIX, file_key, file_source_value, metadata_key, metadata_value};

/// The upstream source for a cached artifact digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSource {
    pub url: String,
    pub source: String,
    pub size: Option<u64>,
}

/// Record the upstream URL a blob digest can be fetched from, and the name of the cached index it came
/// from (so a fetch on a cache miss reuses that index's authentication).
///
/// # Errors
/// Returns a store error if the write fails.
pub fn put_file_url(meta: &MetaStore, sha256: &str, url: &str, source: &str) -> Result<(), MetaError> {
    let value = file_source_value(url, source, None);
    meta.put_driver_value(&file_key(sha256), value.as_bytes())
}

/// Look up the `(upstream url, index name)` for a blob digest.
///
/// # Errors
/// Returns a store error if the read fails.
pub fn get_file_url(meta: &MetaStore, sha256: &str) -> Result<Option<FileSource>, MetaError> {
    Ok(meta
        .get_driver_value(&file_key(sha256))?
        .and_then(|raw| String::from_utf8(raw).ok())
        .and_then(|value| split_file_source(&value)))
}

/// Visit raw file URL records, keyed by artifact digest.
///
/// # Errors
/// Returns a scan error if the store read fails or the visitor returns an error.
pub fn scan_file_urls<E>(
    meta: &MetaStore,
    mut visit: impl FnMut(&str, &str) -> Result<(), E>,
) -> Result<(), MetaScanError<E>> {
    for key in meta.driver_prefix_keys(FILE_PREFIX)? {
        let (Some(digest), Some(raw)) = (key.strip_prefix(FILE_PREFIX), meta.get_driver_value(&key)?) else {
            continue;
        };
        let Ok(value) = String::from_utf8(raw) else {
            continue;
        };
        visit(digest, &value).map_err(MetaScanError::Visit)?;
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

/// Look up an artifact's metadata sibling: `(upstream url, metadata sha256, index name)`.
///
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

/// Look up metadata sha256 values for many artifact digests.
///
/// # Errors
/// Returns a store error if the read fails.
pub fn get_metadata_digests<'a>(
    meta: &MetaStore,
    artifact_sha256s: impl IntoIterator<Item = &'a str>,
) -> Result<HashMap<String, String>, MetaError> {
    let mut metadata = HashMap::new();
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
        let (Some(digest), Some(raw)) = (key.strip_prefix(METADATA_PREFIX), meta.get_driver_value(&key)?) else {
            continue;
        };
        let Ok(value) = String::from_utf8(raw) else {
            continue;
        };
        visit(digest, &value).map_err(MetaScanError::Visit)?;
    }
    Ok(())
}

fn split_file_source(value: &str) -> Option<FileSource> {
    let mut parts = value.splitn(3, '\n');
    Some(FileSource {
        url: parts.next()?.to_owned(),
        source: parts.next()?.to_owned(),
        size: parts.next().and_then(|size| size.parse().ok()),
    })
}
