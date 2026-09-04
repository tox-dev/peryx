use peryx_driver::AppState;
use peryx_driver::serving::{BlobReferenceDriver as _, FsckDriver as _, TrashDriver as _};
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

use super::OciRegistry;

#[test]
fn registry_exposes_oci_maintenance_capabilities() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = AppState::new(
        MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        BlobStore::new(dir.path().join("blobs")),
        60,
        vec![Index {
            name: "images".to_owned(),
            route: "images".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Hosted { volatile: true },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        }],
    );
    peryx_plugin_registry::PluginRegistry::new(vec![crate::registration()])
        .unwrap()
        .activate([crate::ECOSYSTEM])
        .unwrap()
        .install_drivers(
            &mut state.runtime_install_context().unwrap(),
            &std::collections::HashMap::new(),
        )
        .unwrap();

    assert_eq!(
        state
            .idle_reclaimers()
            .map(|(ecosystem, _)| ecosystem.clone())
            .collect::<Vec<_>>(),
        vec![crate::ECOSYSTEM]
    );
    assert_eq!(
        state
            .intent_finalizers()
            .map(|(ecosystem, _)| ecosystem.clone())
            .collect::<Vec<_>>(),
        vec![crate::ECOSYSTEM]
    );
    assert_eq!(state.cache_refreshers().count(), 0);
}

#[test]
fn registry_exposes_storage_contracts() {
    let registry = OciRegistry::default();
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();

    assert!(registry.referenced_blob_digests(&meta).unwrap().is_empty());
    assert!(
        registry
            .trash_records(&meta, &["private".to_owned()])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn registry_fsck_accepts_consistent_oci_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let config = BlobStore::new(dir.path().join("blobs")).write(b"config").unwrap();
    let child = manifest(b"{\"schemaVersion\":2}");
    let child_digest = digest(&child);
    crate::store::record_manifest(&meta, "images", "app", &child_digest, &child).unwrap();
    let index =
        manifest(format!("{{\"schemaVersion\":2,\"manifests\":[{{\"digest\":\"{child_digest}\"}}]}}").as_bytes());
    let index_digest = digest(&index);
    crate::store::record_manifest(&meta, "images", "app", &index_digest, &index).unwrap();
    let image = manifest(
        format!(
            "{{\"schemaVersion\":2,\"config\":{{\"digest\":\"sha256:{}\"}},\"layers\":[]}}",
            config.as_str()
        )
        .as_bytes(),
    );
    crate::store::record_manifest(&meta, "images", "app", &digest(&image), &image).unwrap();
    crate::store::put_tag(&meta, "images", "app", "latest", &index_digest).unwrap();
    let mut output = Vec::new();

    assert_eq!(
        (
            OciRegistry::default()
                .fsck_metadata(
                    &meta,
                    &peryx_storage::blob::BlobStorage::filesystem(dir.path().join("blobs")),
                    &[],
                    &mut output,
                )
                .unwrap(),
            output,
        ),
        (0, Vec::new())
    );
}

#[test]
fn registry_fsck_reports_corrupt_manifests() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    crate::store::record_manifest(
        &meta,
        "images",
        "app",
        &format!("sha256:{}", "a".repeat(64)),
        &manifest(b"not json"),
    )
    .unwrap();
    meta.put_driver_value(&format!("oci\0m\0sha256:{}", "b".repeat(64)), b"invalid")
        .unwrap();
    let mut output = Vec::new();

    assert_eq!(
        OciRegistry::default()
            .fsck_metadata(
                &meta,
                &peryx_storage::blob::BlobStorage::filesystem(dir.path().join("blobs")),
                &[],
                &mut output,
            )
            .unwrap(),
        3
    );
    assert_eq!(
        String::from_utf8(output).unwrap(),
        format!(
            "metadata\toci\tmanifest\tsha256:{}\tdigest mismatch\n\
             metadata\toci\tmanifest\tsha256:{}\tinvalid document\n\
             metadata\toci\tmanifest\tsha256:{}\tinvalid record\n",
            "a".repeat(64),
            "a".repeat(64),
            "b".repeat(64)
        )
    );
}

fn blobs(dir: &tempfile::TempDir) -> peryx_storage::blob::BlobStorage {
    peryx_storage::blob::BlobStorage::filesystem(dir.path().join("blobs"))
}

/// A check that cannot read the manifests has not found the metadata clean, so it reports the read
/// failure rather than the zero problems it happens to have counted.
#[test]
fn registry_fsck_surfaces_a_failure_reading_manifests() {
    let dir = tempfile::tempdir().unwrap();
    let (pages, fault) = peryx_test_support::fault::backend();
    let meta = MetaStore::open_backend(peryx_test_support::fault::faulted(&pages, &fault)).unwrap();
    let mut output = Vec::new();
    fault.arm(0);

    let error = OciRegistry::default()
        .fsck_metadata(&meta, &blobs(&dir), &[], &mut output)
        .unwrap_err();

    assert!(error.contains("injected storage failure"), "{error}");
    assert!(output.is_empty(), "{output:?}");
}

/// A descriptor check that cannot stat the blob store has not established the layer is absent, so
/// the storage failure reaches the operator instead of a "missing blob" finding it did not verify.
#[test]
fn registry_fsck_surfaces_a_failure_reading_blobs() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let layer = format!("sha256:{}", "c".repeat(64));
    let image = manifest(format!("{{\"layers\":[{{\"digest\":\"{layer}\"}}]}}").as_bytes());
    crate::store::record_manifest(&meta, "images", "app", &digest(&image), &image).unwrap();
    // A regular file where the blob root belongs, so every path under it fails as not a directory.
    let root = dir.path().join("blobs");
    std::fs::write(&root, b"not a directory").unwrap();
    let mut output = Vec::new();

    let error = OciRegistry::default()
        .fsck_metadata(
            &meta,
            &peryx_storage::blob::BlobStorage::filesystem(root),
            &[],
            &mut output,
        )
        .unwrap_err();

    assert!(!error.is_empty());
    assert!(output.is_empty(), "reported a finding it never confirmed: {output:?}");
}

/// The tag scan is a second read, so `arm(0)` never reaches it: the manifest scan consumes the
/// injection first. Sweeping the injection point finds the run that fails during the tags, and
/// since both reads fail with the same text the report is the discriminator - empty means the
/// manifest scan failed, non-empty means the manifest findings were written and the tags then did.
#[test]
fn registry_fsck_surfaces_a_failure_reading_tags() {
    let dir = tempfile::tempdir().unwrap();
    let (pages, fault) = peryx_test_support::fault::backend();
    let meta = MetaStore::open_backend(peryx_test_support::fault::faulted(&pages, &fault)).unwrap();
    meta.put_driver_value(&format!("oci\0m\0sha256:{}", "b".repeat(64)), b"invalid")
        .unwrap();
    crate::store::put_tag(&meta, "images", "app", "latest", &format!("sha256:{}", "c".repeat(64))).unwrap();

    drop(meta);
    // A handle does not survive its own injected failure: reusing one across armings leaves every
    // later sweep step failing in the manifest scan. Each step reopens the retained pages instead.
    let during_tags = (0..128).find(|&fail_after| {
        let meta = MetaStore::reopen_backend(peryx_test_support::fault::faulted(&pages, &fault)).unwrap();
        fault.arm(fail_after);
        let mut output = Vec::new();
        let failed = OciRegistry::default()
            .fsck_metadata(&meta, &blobs(&dir), &[], &mut output)
            .is_err();
        fault.disable();
        failed && !output.is_empty()
    });

    assert!(
        during_tags.is_some(),
        "no injection point failed after a manifest finding was written"
    );
}

/// A finding an operator never receives is not a finding, so a report that cannot be written fails
/// the check instead of counting the problem and returning success.
#[test]
fn registry_fsck_surfaces_a_failure_writing_a_report() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    meta.put_driver_value(&format!("oci\0m\0sha256:{}", "b".repeat(64)), b"invalid")
        .unwrap();

    let error = OciRegistry::default()
        // A zero-capacity cursor rather than a writer of our own: a hand-rolled one would carry a
        // `flush` nothing on this path calls, which is an uncovered arm of the test itself.
        .fsck_metadata(
            &meta,
            &blobs(&dir),
            &[],
            &mut std::io::Cursor::new(Box::<[u8]>::default()),
        )
        .unwrap_err();

    assert!(error.contains("failed to write whole buffer"), "{error}");
}

#[test]
fn registry_fsck_reports_missing_descriptor_content_and_tag_targets() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let missing = format!("sha256:{}", "c".repeat(64));
    let index =
        manifest(format!("{{\"manifests\":[{{\"digest\":\"sha512:bad\"}},{{\"digest\":\"{missing}\"}}]}}").as_bytes());
    let index_digest = digest(&index);
    crate::store::record_manifest(&meta, "images", "app", &index_digest, &index).unwrap();
    let image = manifest(
        format!("{{\"config\":{{\"digest\":\"sha512:bad\"}},\"layers\":[{{\"digest\":\"{missing}\"}}]}}").as_bytes(),
    );
    let image_digest = digest(&image);
    crate::store::record_manifest(&meta, "images", "app", &image_digest, &image).unwrap();
    crate::store::put_tag(&meta, "images", "app", "invalid", "sha512:bad").unwrap();
    crate::store::put_tag(&meta, "images", "app", "missing", &missing).unwrap();
    meta.put_driver_value("oci\0t\0images\0app\0bytes", &[0xff]).unwrap();
    let mut output = Vec::new();

    assert_eq!(
        OciRegistry::default()
            .fsck_metadata(
                &meta,
                &peryx_storage::blob::BlobStorage::filesystem(dir.path().join("blobs")),
                &[],
                &mut output,
            )
            .unwrap(),
        7
    );
    assert_eq!(
        String::from_utf8(output).unwrap(),
        format!(
            "metadata\toci\tdescriptor\t{image_digest}\tinvalid blob sha512:bad\n\
             metadata\toci\tdescriptor\t{image_digest}\tmissing blob {missing}\n\
             metadata\toci\tdescriptor\t{index_digest}\tinvalid child manifest sha512:bad\n\
             metadata\toci\tdescriptor\t{index_digest}\tmissing child manifest {missing}\n\
             metadata\toci\ttag\timages/app/bytes\tinvalid record\n\
             metadata\toci\ttag\timages/app/invalid\tinvalid manifest digest\n\
             metadata\toci\ttag\timages/app/missing\tmissing manifest {missing}\n"
        )
    );
}

fn manifest(bytes: &[u8]) -> crate::store::Manifest {
    crate::store::Manifest {
        media_type: "application/vnd.oci.image.manifest.v1+json".to_owned(),
        bytes: bytes.to_vec(),
    }
}

fn digest(manifest: &crate::store::Manifest) -> String {
    format!("sha256:{}", peryx_storage::blob::Digest::of(&manifest.bytes).as_str())
}
