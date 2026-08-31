use super::super::manifest_key;
use super::*;
use crate::store::record_manifest;

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
    record_manifest(
        &meta,
        "hub",
        "app",
        &child,
        &manifest(format!(
            r#"{{"config":{{"digest":"sha256:{a}"}},"layers":[{{"digest":"sha256:{b}"}},{{"digest":"garbage"}}]}}"#,
            a = hex('a'),
            b = hex('b'),
        )),
    )
    .unwrap();
    record_manifest(
        &meta,
        "hub",
        "app",
        &format!("sha256:{}", hex('d')),
        &manifest(format!(r#"{{"manifests":[{{"digest":"{child}"}}]}}"#)),
    )
    .unwrap();
    meta.put_driver_value(&manifest_key(&format!("sha256:{}", hex('e'))), &[0x00])
        .unwrap();

    assert_eq!(
        referenced_blob_digests(&meta).unwrap(),
        BTreeSet::from([hex('a'), hex('b')])
    );
}

#[test]
fn test_referenced_blob_digests_keeps_repository_members() {
    let (_dir, meta) = store();
    let digest = format!("sha256:{}", "f".repeat(64));
    super::super::record_blob_membership(&meta, "store", "app", &digest).unwrap();

    assert_eq!(
        referenced_blob_digests(&meta).unwrap(),
        BTreeSet::from(["f".repeat(64)])
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

const IMAGE_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const INDEX_TYPE: &str = "application/vnd.oci.image.index.v1+json";

fn descriptor(digest: &str) -> String {
    format!(r#"{{"mediaType":"application/octet-stream","digest":"{digest}","size":1}}"#)
}

#[test]
fn test_validated_descriptors_leaves_an_unknown_media_type_opaque() {
    let body = format!(r#"{{"layers":[{}]}}"#, descriptor("sha256:c0ffee"));

    assert_eq!(
        validated_descriptors("application/vnd.example.artifact+json", body.as_bytes()).unwrap(),
        (Vec::new(), Vec::new())
    );
}

#[test]
fn test_validated_descriptors_reports_the_rule_the_body_breaks() {
    let fault = validated_descriptors(IMAGE_TYPE, b"{}").unwrap_err();

    assert_eq!(fault.to_string(), "manifest schemaVersion must be 2");
}

/// The declared schema decides the split, so an image manifest carrying a stray `manifests` array
/// still names the config and layers a client will pull rather than children it does not have.
#[test]
fn test_validated_descriptors_splits_by_the_declared_schema() {
    let hex = |byte: char| format!("sha256:{}", byte.to_string().repeat(64));
    let body = format!(
        r#"{{"schemaVersion":2,"config":{},"layers":[{}],"manifests":[{}]}}"#,
        descriptor(&hex('a')),
        descriptor(&hex('b')),
        descriptor(&hex('c')),
    );

    assert_eq!(
        validated_descriptors(IMAGE_TYPE, body.as_bytes()).unwrap(),
        (Vec::new(), vec![hex('a'), hex('b')])
    );
}

#[test]
fn test_validated_descriptors_names_the_children_of_an_index() {
    let hex = |byte: char| format!("sha256:{}", byte.to_string().repeat(64));
    let body = format!(r#"{{"schemaVersion":2,"manifests":[{}]}}"#, descriptor(&hex('d')));

    assert_eq!(
        validated_descriptors(INDEX_TYPE, body.as_bytes()).unwrap(),
        (vec![hex('d')], Vec::new())
    );
}
