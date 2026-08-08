use super::super::manifest_key;
use super::super::test_support::put_manifest;
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
