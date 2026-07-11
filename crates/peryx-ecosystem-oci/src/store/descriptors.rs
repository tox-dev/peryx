//! Walking a manifest's descriptor graph: the child manifests and blobs an image references, and
//! the union of every blob the store still needs, so cleanup keeps what is reachable.

use std::collections::BTreeSet;

use peryx_storage::blob::Digest;
use peryx_storage::meta::{MetaError, MetaStore};

use super::MANIFEST_PREFIX;
use super::Manifest;

/// Map an OCI `sha256:<hex>` digest onto the blob store's digest, or `None` for another algorithm the
/// content-addressed store cannot key on.
#[must_use]
pub fn blob_digest(digest: &str) -> Option<Digest> {
    Digest::from_hex(digest.strip_prefix("sha256:")?)
}
/// Split a manifest's bytes into the digests it names.
///
/// The two lists are the child manifests of an image index and the config plus layer blobs of an image
/// manifest. Unparseable bytes name nothing. An index names only children (they carry the blobs); an
/// image manifest names only blobs. A layer carrying `urls` is a foreign (non-distributable) layer the
/// registry never stores, so it is omitted: the spec lets a manifest reference it without the blob
/// present, and the orphan purge must not expect it locally.
#[must_use]
pub fn manifest_descriptors(bytes: &[u8]) -> (Vec<String>, Vec<String>) {
    let Ok(document) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return (Vec::new(), Vec::new());
    };
    if let Some(manifests) = document["manifests"].as_array() {
        let children = manifests
            .iter()
            .filter_map(|entry| entry["digest"].as_str().map(str::to_owned))
            .collect();
        return (children, Vec::new());
    }
    let config = document["config"]["digest"].as_str().map(str::to_owned);
    let layers = document["layers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|layer| layer["urls"].as_array().is_none_or(Vec::is_empty))
        .filter_map(|layer| layer["digest"].as_str().map(str::to_owned));
    (Vec::new(), config.into_iter().chain(layers).collect())
}
/// The digest of the index's `linux/amd64` child image manifest, if it lists one.
///
/// Content negotiation serves this child to a client that will not accept an index (legacy Docker
/// < 17.06). The platform lives on each `manifests[]` entry, which the digest-only
/// [`manifest_descriptors`] split does not carry, so the entries are walked here for their platform.
#[must_use]
pub fn linux_amd64_child(bytes: &[u8]) -> Option<String> {
    let document = serde_json::from_slice::<serde_json::Value>(bytes).ok()?;
    document["manifests"].as_array()?.iter().find_map(|entry| {
        let platform = &entry["platform"];
        (platform["os"] == "linux" && platform["architecture"] == "amd64")
            .then(|| entry["digest"].as_str())?
            .map(str::to_owned)
    })
}
/// Every stored blob digest, as storage hex, that a manifest references across all manifests.
///
/// Iterating every stored manifest and unioning its direct blob descriptors covers the whole graph:
/// an image index's children are themselves stored manifests that contribute their own blobs.
/// Retention and the orphaned-blob purge mark from this set, so a blob absent from it is referenced by
/// nothing.
///
/// # Errors
/// Returns a store error if the scan fails.
pub fn referenced_blob_digests(meta: &MetaStore) -> Result<BTreeSet<String>, MetaError> {
    let mut digests = BTreeSet::new();
    for key in meta.driver_prefix_keys(MANIFEST_PREFIX)? {
        let Some(manifest) = meta.get_driver_value(&key)?.as_deref().and_then(Manifest::decode) else {
            continue;
        };
        for blob in manifest_descriptors(&manifest.bytes).1 {
            if let Some(storage) = blob_digest(&blob) {
                digests.insert(storage.as_str().to_owned());
            }
        }
    }
    Ok(digests)
}

#[cfg(test)]
mod tests {
    use super::super::{manifest_key, put_manifest};
    use super::*;

    fn store() -> (tempfile::TempDir, MetaStore) {
        let dir = tempfile::tempdir().unwrap();
        let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
        (dir, meta)
    }

    #[test]
    fn test_referenced_blob_digests_keeps_config_and_layers_only() {
        let (_dir, meta) = store();
        let hex = |byte: char| byte.to_string().repeat(64);
        let manifest = |bytes: String| Manifest {
            media_type: "application/vnd.oci.image.manifest.v1+json".to_owned(),
            bytes: bytes.into_bytes(),
        };
        let child = format!("sha256:{}", hex('c'));
        put_manifest(
            &meta,
            &child,
            &manifest(format!(
                r#"{{"config":{{"digest":"sha256:{a}"}},"layers":[{{"digest":"sha256:{b}"}},{{"digest":"garbage"}}]}}"#,
                a = hex('a'),
                b = hex('b'),
            )),
        )
        .unwrap();
        put_manifest(
            &meta,
            &format!("sha256:{}", hex('d')),
            &manifest(format!(r#"{{"manifests":[{{"digest":"{child}"}}]}}"#)),
        )
        .unwrap();
        meta.put_driver_value(&manifest_key(&format!("sha256:{}", hex('e'))), &[0x00])
            .unwrap();

        // Config and layer blobs survive; the index's child digest is a manifest not a blob, the
        // unparseable layer digest is dropped, and the corrupt manifest contributes nothing.
        assert_eq!(
            referenced_blob_digests(&meta).unwrap(),
            BTreeSet::from([hex('a'), hex('b')])
        );
    }

    #[test]
    fn test_manifest_descriptors_skips_foreign_layers() {
        let hex = |byte: char| byte.to_string().repeat(64);
        let (children, blobs) = manifest_descriptors(
            format!(
                concat!(
                    r#"{{"config":{{"digest":"sha256:{a}"}},"layers":["#,
                    r#"{{"digest":"sha256:{b}"}},"#,
                    r#"{{"digest":"sha256:{c}","urls":["https://store.example.com/blob"]}}]}}"#,
                ),
                a = hex('a'),
                b = hex('b'),
                c = hex('c'),
            )
            .as_bytes(),
        );
        // The `urls`-bearing foreign layer is omitted; config and the ordinary layer remain.
        assert!(children.is_empty());
        assert_eq!(
            blobs,
            vec![format!("sha256:{}", hex('a')), format!("sha256:{}", hex('b'))]
        );
    }

    #[test]
    fn test_linux_amd64_child_selects_the_matching_platform_digest() {
        let hex = |byte: char| format!("sha256:{}", byte.to_string().repeat(64));
        let index = format!(
            concat!(
                r#"{{"manifests":["#,
                r#"{{"digest":"{arm}","platform":{{"os":"linux","architecture":"arm64"}}}},"#,
                r#"{{"digest":"{amd}","platform":{{"os":"linux","architecture":"amd64"}}}}]}}"#,
            ),
            arm = hex('a'),
            amd = hex('b'),
        );
        assert_eq!(linux_amd64_child(index.as_bytes()), Some(hex('b')));
    }

    #[test]
    fn test_linux_amd64_child_is_none_without_a_matching_entry() {
        let hex = |byte: char| format!("sha256:{}", byte.to_string().repeat(64));
        // Unparseable bytes, a document with no `manifests`, a `linux/amd64` entry missing its digest,
        // and an index whose only child is another platform each yield no child.
        assert_eq!(linux_amd64_child(b"not json"), None);
        assert_eq!(linux_amd64_child(br#"{"schemaVersion":2}"#), None);
        assert_eq!(
            linux_amd64_child(br#"{"manifests":[{"platform":{"os":"linux","architecture":"amd64"}}]}"#),
            None
        );
        assert_eq!(
            linux_amd64_child(
                format!(
                    r#"{{"manifests":[{{"digest":"{}","platform":{{"os":"windows","architecture":"amd64"}}}}]}}"#,
                    hex('c'),
                )
                .as_bytes()
            ),
            None
        );
    }

    #[test]
    fn test_blob_digest_only_maps_sha256() {
        assert!(blob_digest(&format!("sha256:{}", "a".repeat(64))).is_some());
        assert_eq!(blob_digest("sha512:abc"), None);
        assert_eq!(blob_digest("sha256:short"), None);
    }
}
