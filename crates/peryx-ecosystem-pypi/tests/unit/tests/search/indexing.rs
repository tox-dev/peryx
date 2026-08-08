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
async fn test_search_drops_a_project_whose_only_upload_is_trashed() {
    let h = harness().await;
    put_uploaded_package(&h.state, "TrashOnly", "trash-only", "Soft-deleted upload");
    trash_upload(&h.state, "trash-only", "trash-only-1.0-py3-none-any.whl");

    let (status, _headers, body) = get(
        &h.state,
        "/hosted/+search?q=trash-only&type=uploaded&page_size=25",
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["total"], 0);
}
#[tokio::test]
async fn test_search_keeps_a_live_release_when_a_sibling_is_trashed() {
    let h = harness().await;
    put_uploaded_package(&h.state, "MixedPkg", "mixed-pkg", "A partly trashed project");
    let trashed = put_uploaded_file(&h.state, "mixed-pkg", "2.0");
    trash_upload(&h.state, "mixed-pkg", &trashed);

    let (status, _headers, body) = get(
        &h.state,
        "/hosted/+search?q=mixed-pkg&type=uploaded&page_size=25",
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["total"], 1);
    assert_eq!(value["results"][0]["display_name"], "MixedPkg");
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
            h.state.blobs.put_bytes(&[0xff]).await.unwrap().as_str(),
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
async fn test_search_indexes_a_project_whose_metadata_sibling_is_malformed() {
    let h = harness().await;
    let artifact = Digest::of(b"malformed metadata");
    h.state
        .meta
        .put_metadata(
            artifact.as_str(),
            "uploaded",
            h.state
                .blobs
                .put_bytes(b"Name: malformed\nmalformed header\nVersion: 1.0\n")
                .await
                .unwrap()
                .as_str(),
            "pypi",
        )
        .unwrap();
    put_cached_package(
        &h.state,
        "pypi/malformed",
        "pypi",
        "malformed",
        &ProjectDetail {
            meta: Meta::default(),
            name: "Malformed".to_owned(),
            versions: vec!["1.0".to_owned()],
            files: vec![file_with_hash(
                "malformed-1.0-py3-none-any.whl",
                artifact.as_str(),
                None,
            )],
        },
    );

    let (status, _headers, body) = get(
        &h.state,
        "/pypi/+search?q=malformed&type=cached&page_size=25",
        Some("application/json"),
    )
    .await;

    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["total"], 1);
    assert_eq!(value["results"][0]["display_name"], "Malformed");
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

#[tokio::test]
async fn test_search_indexes_legacy_home_page() {
    let h = harness().await;
    put_uploaded_package_with_metadata(
        &h.state,
        "legacy-home",
        "Metadata-Version: 2.1\nName: legacy-home\nVersion: 1.0\nHome-Page: https://legacy.example/project\n",
        None,
    );

    let (status, _headers, body) = get(
        &h.state,
        "/hosted/+search?q=legacy.example&page_size=25",
        Some("application/json"),
    )
    .await;
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["total"], 1);
    assert_eq!(value["results"][0]["normalized_name"], "legacy-home");
}

#[tokio::test]
async fn test_search_indexes_import_names_without_markers() {
    let h = harness().await;
    put_uploaded_package_with_metadata(
        &h.state,
        "importable",
        "Metadata-Version: 2.5\nName: importable\nVersion: 1.0\n\
         Import-Name: veloximport; private\nImport-Namespace: shared_ns\n",
        None,
    );

    for query in ["veloximport", "shared_ns"] {
        let (status, _headers, body) = get(
            &h.state,
            &format!("/hosted/+search?q={query}&page_size=25"),
            Some("application/json"),
        )
        .await;
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["total"], 1, "{query}");
        assert_eq!(value["results"][0]["normalized_name"], "importable");
    }
}
#[tokio::test]
async fn test_search_availability_filter_keeps_locally_available_projects() {
    use peryx_storage::meta::ArtifactSource;

    let h = harness().await;
    // A mirrored project whose file was never fetched stays remote-only.
    put_cached_package(
        &h.state,
        "pypi/remote-dist",
        "pypi",
        "remote-dist",
        &ProjectDetail {
            meta: Meta::default(),
            name: "remote-dist".to_owned(),
            versions: vec!["1.0".to_owned()],
            files: vec![file_with_hash(
                "remote-dist-1.0-py3-none-any.whl",
                Digest::of(b"remote-dist").as_str(),
                None,
            )],
        },
    );
    // A mirrored project whose file's verified bytes are held locally.
    put_cached_package(
        &h.state,
        "pypi/local-dist",
        "pypi",
        "local-dist",
        &ProjectDetail {
            meta: Meta::default(),
            name: "local-dist".to_owned(),
            versions: vec!["1.0".to_owned()],
            files: vec![file_with_hash(
                "local-dist-1.0-py3-none-any.whl",
                Digest::of(b"local-dist").as_str(),
                None,
            )],
        },
    );
    h.state
        .meta
        .record_artifact_placement(Digest::of(b"local-dist").as_str(), ArtifactSource::Proxy, true)
        .unwrap();
    h.state.bump_search_epoch();

    let (status, _headers, body) = get(
        &h.state,
        "/pypi/+search?q=dist&type=cached&availability=all&page_size=25",
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let all: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(all["availability"], "all");
    assert_eq!(all["total"], 2);
    let available: BTreeMap<&str, bool> = all["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|result| {
            (
                result["normalized_name"].as_str().unwrap(),
                result["available"].as_bool().unwrap(),
            )
        })
        .collect();
    assert!(available["local-dist"]);
    assert!(!available["remote-dist"]);

    let (status, _headers, body) = get(
        &h.state,
        "/pypi/+search?q=dist&type=cached&availability=local&page_size=25",
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let local: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(local["availability"], "local");
    assert_eq!(local["total"], 1);
    assert_eq!(local["results"][0]["normalized_name"], "local-dist");
    assert_eq!(local["results"][0]["available"], true);
}

#[tokio::test]
async fn test_search_availability_local_includes_hosted_uploads() {
    let h = harness().await;
    put_uploaded_package(&h.state, "HostedPkg", "hosted-pkg", "A hosted upload");

    // An upload's bytes are local, so it survives the local-availability filter without a placement row.
    let (status, _headers, body) = get(
        &h.state,
        "/hosted/+search?q=hosted-pkg&type=uploaded&availability=local&page_size=25",
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["total"], 1);
    assert_eq!(value["results"][0]["normalized_name"], "hosted-pkg");
    assert_eq!(value["results"][0]["available"], true);
}

#[tokio::test]
async fn test_search_availability_excludes_an_evicted_hosted_upload() {
    use peryx_storage::meta::ArtifactSource;

    let h = harness().await;
    put_uploaded_package(&h.state, "EvictedPkg", "evicted-pkg", "A hosted upload since evicted");
    // The upload's own hosted-source placement records its bytes as gone, so the file view and search
    // both stop calling it locally available even though it is a hosted file.
    let digest = Digest::of(b"evicted-pkg-1.0-py3-none-any.whl");
    h.state
        .meta
        .record_artifact_placement(digest.as_str(), ArtifactSource::Hosted, false)
        .unwrap();
    h.state.bump_search_epoch();

    let (status, _headers, body) = get(
        &h.state,
        "/hosted/+search?q=evicted-pkg&type=uploaded&availability=all&page_size=25",
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let all: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(all["total"], 1);
    assert_eq!(all["results"][0]["available"], false);

    let (status, _headers, body) = get(
        &h.state,
        "/hosted/+search?q=evicted-pkg&type=uploaded&availability=local&page_size=25",
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["total"], 0);
}

#[tokio::test]
async fn test_search_rejects_an_unknown_availability_filter() {
    let h = harness().await;
    let (status, _headers, body) = get(&h.state, "/pypi/+search?availability=maybe", Some("application/json")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["error"], "invalid availability filter \"maybe\"");
}

/// A quarantining member of a virtual index must withhold the merged files from search whatever its listed
/// order, so a benign sibling never leaks a project a quarantine should hide. Reverting the merge in
/// `virtual_detail` back to a benign-wins rule makes the `[0, 1]` case index the served files and fail.
async fn assert_search_quarantine_dominates(layers: Vec<usize>) {
    let (_dir, state) = two_cached_virtual_state(layers);
    put_cached_package(
        &state,
        "archived/peryxpkg",
        "archived",
        "peryxpkg",
        &ProjectDetail {
            meta: meta_status("archived", "sunset"),
            name: "peryxpkg".to_owned(),
            versions: vec!["1.0".to_owned()],
            files: vec![file_with_hash("peryxpkg-1.0-py3-none-any.whl", &"a".repeat(64), None)],
        },
    );
    put_cached_package(
        &state,
        "quarantined/peryxpkg",
        "quarantined",
        "peryxpkg",
        &ProjectDetail {
            meta: meta_status("quarantined", "waiting period"),
            name: "peryxpkg".to_owned(),
            versions: vec!["1.0".to_owned()],
            files: vec![file_with_hash("peryxpkg-1.0-py3-none-any.whl", &"b".repeat(64), None)],
        },
    );

    let (status, _headers, body) = get(
        &state,
        "/root/pypi/+search?q=peryxpkg&page_size=25",
        Some("application/json"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["total"],
        0,
        "a quarantining member must withhold the virtual index's files whatever its order: {body}"
    );
}
#[tokio::test]
async fn test_search_virtual_quarantine_dominates_when_listed_after_a_benign_member() {
    assert_search_quarantine_dominates(vec![0, 1]).await;
}
#[tokio::test]
async fn test_search_virtual_quarantine_dominates_when_listed_before_a_benign_member() {
    assert_search_quarantine_dominates(vec![1, 0]).await;
}
