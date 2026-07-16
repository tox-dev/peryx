use std::collections::BTreeMap;

use peryx_storage::meta::{MetaError, MetaScanError, MetaStore};

use super::{FILE_PREFIX, METADATA_PREFIX, file_key, file_source_value, metadata_key, metadata_value};

/// The upstream source for a cached artifact digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSource {
    pub url: String,
    pub source: String,
    pub size: Option<u64>,
    /// The named routed upstream that advertised this artifact.
    pub upstream: Option<String>,
}

/// Record the upstream URL a blob digest can be fetched from, and the name of the cached index it came
/// from (so a fetch on a cache miss reuses that index's authentication).
///
/// # Errors
/// Returns a store error if the write fails.
pub fn put_file_url(meta: &MetaStore, sha256: &str, url: &str, source: &str) -> Result<(), MetaError> {
    let value = file_source_value(url, source, None, None);
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
mod tests {
    use std::collections::BTreeMap;

    use super::{FileSource, MetaStore, metadata_key, split_file_source};
    use crate::store::PypiStore as _;

    fn store() -> (tempfile::TempDir, MetaStore) {
        let dir = tempfile::tempdir().unwrap();
        let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
        (dir, meta)
    }

    #[test]
    fn test_put_and_get_file_url() {
        let (_dir, meta) = store();
        assert_eq!(meta.get_file_url("deadbeef").unwrap(), None);
        meta.put_file_url("deadbeef", "https://files.example/pkg.whl", "pypi")
            .unwrap();
        assert_eq!(
            meta.get_file_url("deadbeef").unwrap(),
            Some(FileSource {
                url: "https://files.example/pkg.whl".to_owned(),
                source: "pypi".to_owned(),
                size: None,
                upstream: None,
            })
        );
    }

    #[test]
    fn test_file_source_without_size_keeps_routed_upstream() {
        assert_eq!(
            split_file_source("https://files.example/pkg.whl\npypi\n\nmirror"),
            Some(FileSource {
                url: "https://files.example/pkg.whl".to_owned(),
                source: "pypi".to_owned(),
                size: None,
                upstream: Some("mirror".to_owned()),
            })
        );
    }

    #[test]
    fn test_put_and_get_metadata_roundtrips_the_sibling() {
        let (_dir, meta) = store();
        assert_eq!(meta.get_metadata("wheelsha").unwrap(), None);
        meta.put_metadata("wheelsha", "https://up/pkg.whl.metadata", "metasha", "pypi")
            .unwrap();
        assert_eq!(
            meta.get_metadata("wheelsha").unwrap(),
            Some((
                "https://up/pkg.whl.metadata".to_owned(),
                "metasha".to_owned(),
                "pypi".to_owned(),
            ))
        );
    }

    #[test]
    fn test_get_metadata_digests_skips_missing_and_malformed_records() {
        let (_dir, meta) = store();
        meta.put_metadata("wheelsha", "https://up/pkg.whl.metadata", "metasha", "pypi")
            .unwrap();
        // A record with no newline lacks the sha256 field, so the lookup skips it rather than panicking.
        meta.put_driver_value(&metadata_key("broken"), b"only-url").unwrap();

        let digests = meta.get_metadata_digests(["missing", "broken", "wheelsha"]).unwrap();

        assert_eq!(digests, BTreeMap::from([("wheelsha".to_owned(), "metasha".to_owned())]));
    }

    #[test]
    fn test_scan_file_urls_visits_each_record() {
        let (_dir, meta) = store();
        meta.put_file_url("aa", "https://files/aa.whl", "pypi").unwrap();
        let mut seen = Vec::new();
        meta.scan_file_urls(|digest, value| {
            seen.push((digest.to_owned(), value.to_owned()));
            Ok::<(), std::io::Error>(())
        })
        .unwrap();
        assert_eq!(seen, vec![("aa".to_owned(), "https://files/aa.whl\npypi".to_owned())]);
    }

    #[test]
    fn test_scan_file_urls_skips_a_non_utf8_record() {
        let (_dir, meta) = store();
        meta.put_file_url("aa", "https://files/aa.whl", "pypi").unwrap();
        meta.put_driver_value(&super::file_key("bad"), &[0xff, 0xfe]).unwrap();
        let mut count = 0;
        meta.scan_file_urls(|_digest, _value| {
            count += 1;
            Ok::<(), std::io::Error>(())
        })
        .unwrap();
        assert_eq!(count, 1, "the non-UTF-8 record is skipped, the valid one visited");
    }

    #[test]
    fn test_scan_metadata_records_visits_each_record() {
        let (_dir, meta) = store();
        meta.put_metadata("wheelsha", "https://up/pkg.metadata", "metasha", "pypi")
            .unwrap();
        let mut seen = Vec::new();
        meta.scan_metadata_records(|digest, value| {
            seen.push((digest.to_owned(), value.to_owned()));
            Ok::<(), std::io::Error>(())
        })
        .unwrap();
        assert_eq!(
            seen,
            vec![(
                "wheelsha".to_owned(),
                "https://up/pkg.metadata\nmetasha\npypi".to_owned()
            )]
        );
    }

    #[test]
    fn test_scan_metadata_records_skips_a_non_utf8_record() {
        let (_dir, meta) = store();
        meta.put_metadata("good", "https://up/pkg.metadata", "metasha", "pypi")
            .unwrap();
        meta.put_driver_value(&metadata_key("bad"), &[0xff, 0xfe]).unwrap();
        let mut seen = Vec::new();
        meta.scan_metadata_records(|digest, _value| {
            seen.push(digest.to_owned());
            Ok::<(), std::io::Error>(())
        })
        .unwrap();
        assert_eq!(seen, vec!["good".to_owned()], "the non-UTF-8 record is skipped");
    }
}
