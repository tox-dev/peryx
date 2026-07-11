//! What the search index ingests from stored records.

use super::support::*;

#[tokio::test]
async fn test_search_indexes_uploaded_metadata_and_route_scope() {
    let h = harness().await;
    put_uploaded_package(
        &h.state,
        "PeryxPkg",
        "peryxpkg",
        "Fast package cache for Python indexes",
    );

    let (status, _headers, body) = get(
        &h.state,
        "/hosted/+search?q=package%20cache&type=uploaded&page_size=25",
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["total"], 1);
    assert_eq!(value["route"], "hosted");
    assert_eq!(value["results"][0]["display_name"], "PeryxPkg");
    assert_eq!(value["results"][0]["normalized_name"], "peryxpkg");
    assert_eq!(value["results"][0]["type"], "uploaded");
}
#[tokio::test]
async fn test_search_collects_direct_mirror_and_local_projects() {
    let h = harness().await;
    put_cached_package(
        &h.state,
        "pypi/direct-mirror",
        "pypi",
        "direct-mirror",
        &ProjectDetail {
            meta: Meta::default(),
            name: "DirectMirror".to_owned(),
            versions: vec!["1.0".to_owned()],
            files: vec![file_with_hash(
                "direct-mirror-1.0-py3-none-any.whl",
                Digest::of(b"direct-mirror").as_str(),
                None,
            )],
        },
    );
    put_uploaded_package(&h.state, "LocalOnly", "local-only", "Local search package");

    let (status, _headers, body) = get(
        &h.state,
        "/pypi/+search?q=direct-mirror&type=cached&page_size=25",
        Some("application/json"),
    )
    .await;
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["total"], 1);
    assert_eq!(value["results"][0]["display_name"], "DirectMirror");

    let (status, _headers, body) = get(
        &h.state,
        "/hosted/+search?q=local-only&type=uploaded&page_size=25",
        Some("application/json"),
    )
    .await;
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["total"], 1);
    assert_eq!(value["results"][0]["display_name"], "LocalOnly");
}
#[tokio::test]
async fn test_search_skips_unusable_metadata_and_quarantined_projects() {
    let h = harness().await;
    let invalid_hex = Digest::of(b"invalid metadata digest");
    let missing_blob = Digest::of(b"missing metadata blob");
    let invalid_utf8 = Digest::of(b"invalid metadata utf8");
    let missing_metadata = Digest::of(b"missing metadata");
    h.state
        .meta
        .put_metadata(invalid_hex.as_str(), "uploaded", "not-hex", "pypi")
        .unwrap();
    h.state
        .meta
        .put_metadata(
            invalid_utf8.as_str(),
            "uploaded",
            h.state.blobs.write(&[0xff]).unwrap().as_str(),
            "pypi",
        )
        .unwrap();
    h.state
        .meta
        .put_metadata(missing_blob.as_str(), "uploaded", missing_metadata.as_str(), "pypi")
        .unwrap();
    put_cached_package(
        &h.state,
        "pypi/metadata-skips",
        "pypi",
        "metadata-skips",
        &ProjectDetail {
            meta: Meta::default(),
            name: String::new(),
            versions: vec!["1.0".to_owned()],
            files: vec![
                file_with_hash(
                    "metadata-skips-1.0-py3-none-invalid-utf8.whl",
                    invalid_utf8.as_str(),
                    Some(">=3.11"),
                ),
                file_with_hash(
                    "metadata-skips-1.0-py3-none-missing-blob.whl",
                    missing_blob.as_str(),
                    None,
                ),
                file_with_hash(
                    "metadata-skips-1.0-py3-none-invalid-hex.whl",
                    invalid_hex.as_str(),
                    None,
                ),
                File {
                    filename: "metadata-skips-1.0.tar.gz".to_owned(),
                    url: "https://files.example/metadata-skips-1.0.tar.gz".to_owned(),
                    hashes: BTreeMap::new(),
                    requires_python: None,
                    size: Some(10),
                    upload_time: None,
                    yanked: Yanked::No,
                    core_metadata: CoreMetadata::Hashes(BTreeMap::from([(
                        "sha256".to_owned(),
                        Digest::of(b"unused").as_str().to_owned(),
                    )])),
                    dist_info_metadata: CoreMetadata::Absent,
                    gpg_sig: None,
                    provenance: Provenance::Absent,
                },
            ],
        },
    );
    put_cached_package(
        &h.state,
        "pypi/quarantined",
        "pypi",
        "quarantined",
        &ProjectDetail {
            meta: meta_status("quarantined", "waiting period"),
            name: "Quarantined".to_owned(),
            versions: vec!["1.0".to_owned()],
            files: vec![file_with_hash(
                "quarantined-1.0-py3-none-any.whl",
                Digest::of(b"quarantined").as_str(),
                None,
            )],
        },
    );

    let (status, _headers, body) = get(
        &h.state,
        "/pypi/+search?q=metadata-skips&type=cached&page_size=25",
        Some("application/json"),
    )
    .await;
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["total"], 1);
    assert_eq!(value["results"][0]["display_name"], "metadata-skips");

    let (status, _headers, body) = get(
        &h.state,
        "/pypi/+search?q=quarantined&type=cached&page_size=25",
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["total"], 0);
}
#[tokio::test]
async fn test_search_indexes_metadata_field_lists_and_long_text() {
    let h = harness().await;
    put_uploaded_package_with_metadata(
        &h.state,
        "longtext",
        &format!(
            "Metadata-Version: 2.4\n\
             Name: {}\n\
             Version: 1.0\n\
             Summary: metadata fields\n\
             Requires-Python: >=3.11\n\
             License: MIT\n\
             License-Expression: MIT\n\
             Author: Ada Lovelace\n\
             Maintainer: Release Team\n\
             Description-Content-Type: text/markdown\n\
             Keywords: async,cache\n\
             Requires-Dist: rich>=13\n\
             Provides-Extra: docs\n\
             Classifier: Topic :: Software Development :: Libraries\n\
             License-File: LICENSE\n\
             Project-URL: Documentation, https://docs.example/longtext\n\
             \n\
             Package description",
            "€".repeat(11_000)
        ),
        Some(">=3.11"),
    );

    let (status, _headers, body) = get(
        &h.state,
        "/hosted/+search?q=docs.example&page_size=25",
        Some("application/json"),
    )
    .await;
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["total"], 1);
    assert_eq!(value["results"][0]["normalized_name"], "longtext");
    assert_eq!(value["results"][0]["summary"], "metadata fields");
}
#[test]
fn test_search_public_filter_labels_and_scan_errors() {
    let dir = tempfile::tempdir().unwrap();
    PackageSearch::open(dir.path()).unwrap();
    assert_eq!(
        [
            SourceFilter::All.as_str(),
            SourceFilter::Uploaded.as_str(),
            SourceFilter::Cached.as_str(),
            SourceFilter::Override.as_str(),
        ],
        ["all", "uploaded", "cached", "override"]
    );
    assert_eq!(SourceFilter::from_value("blocked"), None);
    assert_eq!(
        [
            PackageSource::Uploaded.label(),
            PackageSource::Cached.label(),
            PackageSource::Override.label(),
        ],
        ["Uploaded", "Cached", "Override"]
    );
    assert_eq!(PackageSource::from_value("blocked"), None);
    assert!(matches!(
        SearchError::from(MetaScanError::Visit(SearchError::InvalidSource("blocked".to_owned()))),
        SearchError::InvalidSource(_)
    ));
    assert!(matches!(
        SearchError::from(MetaScanError::Store(meta_error())),
        SearchError::Meta(_)
    ));
}
