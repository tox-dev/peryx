use super::{Manifest, manifest_key};
use peryx_storage::meta::{MetaError, MetaStore};

pub fn put_manifest(meta: &MetaStore, digest: &str, manifest: &Manifest) -> Result<(), MetaError> {
    meta.put_driver_value(&manifest_key(digest), &manifest.encode())
}

pub fn delete_manifest(meta: &MetaStore, digest: &str) -> Result<bool, MetaError> {
    meta.delete_driver_value(&manifest_key(digest))
}
