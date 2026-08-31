//! Walking a manifest's descriptor graph: the child manifests and blobs an image references, and
//! the union of every blob the store still needs, so cleanup keeps what is reachable.

use std::collections::BTreeSet;

use peryx_storage::blob::Digest;
use peryx_storage::meta::{MetaError, MetaStore};

use super::{BLOB_MEMBERSHIP_PREFIX, MANIFEST_PREFIX, Manifest, ManifestSchema, ManifestSchemaError};

/// What a manifest depends on: the child manifests of an image index, then the config and layer blobs
/// of an image manifest.
pub type Descriptors = (Vec<String>, Vec<String>);

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
pub fn manifest_descriptors(bytes: &[u8]) -> Descriptors {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .as_ref()
        .map_or_else(|_| (Vec::new(), Vec::new()), document_descriptors)
}
/// The same split for a document the caller has already parsed, so a pushed manifest is read once.
#[must_use]
pub fn document_descriptors(document: &serde_json::Value) -> Descriptors {
    if document["manifests"].is_array() {
        return (child_manifests(document), Vec::new());
    }
    (Vec::new(), image_blobs(document))
}
/// Check a manifest against the schema its media type declares, then split the descriptors of the
/// document that passed, so bytes peryx stores are bytes it can serve back and walk.
///
/// A media type peryx models no schema for is opaque: its body is asserted nothing about and it names
/// no dependencies, so an artifact type peryx does not understand neither fails a mirror run nor sends
/// it after blobs the manifest never named. The split follows the declared schema rather than the
/// fields that happen to be present, so an image manifest carrying a stray `manifests` array still
/// names the config and layers a client will pull.
///
/// # Errors
/// Returns the first rule the document breaks.
pub fn validated_descriptors(media_type: &str, bytes: &[u8]) -> Result<Descriptors, ManifestSchemaError> {
    let Some(schema) = ManifestSchema::of(media_type) else {
        return Ok((Vec::new(), Vec::new()));
    };
    let document = schema.parse(bytes)?;
    Ok(match schema {
        ManifestSchema::Index => (child_manifests(&document), Vec::new()),
        ManifestSchema::Image => (Vec::new(), image_blobs(&document)),
    })
}

fn child_manifests(document: &serde_json::Value) -> Vec<String> {
    document["manifests"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry["digest"].as_str().map(str::to_owned))
        .collect()
}

fn image_blobs(document: &serde_json::Value) -> Vec<String> {
    let config = document["config"]["digest"].as_str().map(str::to_owned);
    let layers = document["layers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|layer| layer["urls"].as_array().is_none_or(Vec::is_empty))
        .filter_map(|layer| layer["digest"].as_str().map(str::to_owned));
    config.into_iter().chain(layers).collect()
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
    for key in meta.driver_prefix_keys(BLOB_MEMBERSHIP_PREFIX)? {
        if let Some(storage) = key.rsplit_once('\u{0}').and_then(|(_, digest)| blob_digest(digest)) {
            digests.insert(storage.as_str().to_owned());
        }
    }
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
#[path = "../../tests/unit/store/descriptors/tests.rs"]
mod tests;
