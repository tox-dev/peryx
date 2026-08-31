use super::support::*;
use crate::PypiIndexer;
use crate::tests::http::placement_harness;
use peryx_ha::{ArtifactPlacement, ArtifactSource};
use peryx_search::{INDEXED_TEXT_BYTES, IndexerCtx, SearchDocumentProvider as _, SearchError};

const OVERSIZED_CATALOG_FILES: usize = INDEXED_TEXT_BYTES / "large-catalog-1.0-00000-py3-none-any.whl".len() + 1;

fn hosted_search_text(state: &ServingState) -> String {
    PypiIndexer
        .documents(&IndexerCtx {
            indexes: &state.indexes,
            meta: &state.meta,
            blobs: &state.blobs,
        })
        .unwrap()
        .into_iter()
        .find(|document| document.route == "hosted")
        .unwrap()
        .text
}

#[test]
fn test_search_indexer_reports_a_metadata_scan_failure() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("peryx.redb");
    drop(redb::Database::create(&database).unwrap());
    let meta = MetaStore::open_existing(database).unwrap();
    let blobs = BlobStorage::filesystem(directory.path().join("blobs"));
    let indexes = [Index {
        name: "pypi".to_owned(),
        route: "pypi".to_owned(),
        ecosystem: crate::ECOSYSTEM,
        kind: IndexKind::Cached {
            client: UpstreamClient::new("https://example.test/simple/").unwrap(),
            offline: false,
        },
        policy: Policy::default(),
        acl: peryx_identity::IndexAcl::default(),
    }];

    let error = PypiIndexer
        .documents(&IndexerCtx {
            indexes: &indexes,
            meta: &meta,
            blobs: &blobs,
        })
        .err()
        .expect("metadata scan fails without its table");

    assert!(matches!(error, SearchError::Meta(_)));
}

#[tokio::test]
async fn test_search_indexes_uploaded_metadata_and_route_scope() {
    let h = placement_harness().await;
    put_uploaded_package(
        &h.state.serving,
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
    assert_eq!(status, StatusCode::OK, "{body}");
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["total"], 1);
    assert_eq!(value["route"], "hosted");
    assert_eq!(value["results"][0]["display_label"], "PeryxPkg");
    assert_eq!(value["results"][0]["resource_key"], "peryxpkg");
    assert_eq!(value["results"][0]["type"], "uploaded");
}

#[tokio::test]
async fn test_search_document_text_separates_populated_fields() {
    let h = placement_harness().await;
    put_uploaded_package_with_metadata(
        &h.state.serving,
        "boundary-pkg",
        "Metadata-Version: 2.4\nName: BoundaryPkg\nVersion: 1.0\nSummary: alpha\nLicense: beta\n",
        None,
    );

    let text = hosted_search_text(&h.state.serving);

    assert!(text.contains("alpha beta"), "{text}");
}

#[tokio::test]
async fn test_search_document_text_skips_empty_fields() {
    let h = placement_harness().await;
    put_uploaded_package_with_metadata(
        &h.state.serving,
        "boundary-pkg",
        "Metadata-Version: 2.4\nName: BoundaryPkg\nVersion: 1.0\n",
        Some(" "),
    );
    put_uploaded_file(&h.state.serving, "boundary-pkg", "2.0");

    let text = hosted_search_text(&h.state.serving);

    assert!(!text.contains("  "), "{text}");
}

#[rstest::rstest]
#[case::summary_short(1, &["quasarproxy"])]
#[case::all_fields_large(OVERSIZED_CATALOG_FILES, &["quasarproxy", "large-catalog", "catalogneedle"])]
#[tokio::test]
async fn test_search_text_budget_preserves_each_field_class(#[case] file_count: usize, #[case] queries: &[&str]) {
    let h = placement_harness().await;
    put_search_budget_package(&h.state.serving, file_count);

    let mut totals = Vec::with_capacity(queries.len());
    for query in queries.iter().copied() {
        totals.push((
            query,
            search_total(&h.state, &format!("/pypi/+search?q={query}&type=cached&page_size=25")).await,
        ));
    }
    assert_eq!(totals, queries.iter().map(|query| (*query, 1)).collect::<Vec<_>>());
}

fn put_search_budget_package(state: &ServingState, file_count: usize) {
    let files = (0..file_count)
        .map(|index| {
            let filename = if index == 0 {
                "catalogneedle-1.0-py3-none-any.whl".to_owned()
            } else {
                format!("large-catalog-1.0-{index:05}-py3-none-any.whl")
            };
            file_with_hash(&filename, Digest::of(filename.as_bytes()).as_str(), Some(">=3.11"))
        })
        .collect::<Vec<_>>();
    let metadata = state
        .blobs
        .blocking()
        .put_bytes(
            format!(
                "Metadata-Version: 2.1\nName: {}\nVersion: 1.0\nSummary: quasarproxy\n",
                "X".repeat(INDEXED_TEXT_BYTES / 2)
            )
            .as_bytes(),
        )
        .unwrap();
    state
        .meta
        .put_metadata(files.first().unwrap().sha256().unwrap(), metadata.as_str())
        .unwrap();
    put_cached_package(
        state,
        "pypi/large-catalog",
        "pypi",
        "large-catalog",
        &ProjectDetail {
            meta: Meta::default(),
            name: "LargeCatalog".to_owned(),
            versions: vec!["1.0".to_owned()],
            files,
        },
    );
}
#[tokio::test]
async fn test_search_drops_a_project_whose_only_upload_is_trashed() {
    let h = harness().await;
    put_uploaded_package(&h.state.serving, "TrashOnly", "trash-only", "Soft-deleted upload");
    trash_upload(&h.state.serving, "trash-only", "trash-only-1.0-py3-none-any.whl");

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
    let h = placement_harness().await;
    put_uploaded_package(&h.state.serving, "MixedPkg", "mixed-pkg", "A partly trashed project");
    let trashed = put_uploaded_file(&h.state.serving, "mixed-pkg", "2.0");
    trash_upload(&h.state.serving, "mixed-pkg", &trashed);

    let (status, _headers, body) = get(
        &h.state,
        "/hosted/+search?q=mixed-pkg&type=uploaded&page_size=25",
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["total"], 1);
    assert_eq!(value["results"][0]["display_label"], "MixedPkg");
}
#[tokio::test]
async fn test_search_collects_direct_mirror_and_local_projects() {
    let h = placement_harness().await;
    put_cached_package(
        &h.state.serving,
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
    put_uploaded_package(&h.state.serving, "LocalOnly", "local-only", "Local search package");

    let (status, _headers, body) = get(
        &h.state,
        "/pypi/+search?q=direct-mirror&type=cached&page_size=25",
        Some("application/json"),
    )
    .await;
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["total"], 1);
    assert_eq!(value["results"][0]["display_label"], "DirectMirror");

    let (status, _headers, body) = get(
        &h.state,
        "/hosted/+search?q=local-only&type=uploaded&page_size=25",
        Some("application/json"),
    )
    .await;
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["total"], 1);
    assert_eq!(value["results"][0]["display_label"], "LocalOnly");
}
#[tokio::test]
async fn test_search_skips_unusable_metadata_and_quarantined_projects() {
    let h = placement_harness().await;
    let invalid_hex = Digest::of(b"invalid metadata digest");
    let missing_blob = Digest::of(b"missing metadata blob");
    let invalid_utf8 = Digest::of(b"invalid metadata utf8");
    let missing_metadata = Digest::of(b"missing metadata");
    h.state
        .serving
        .meta
        .put_metadata(invalid_hex.as_str(), "not-hex")
        .unwrap();
    h.state
        .serving
        .meta
        .put_metadata(
            invalid_utf8.as_str(),
            h.state.serving.blobs.put_bytes(&[0xff]).await.unwrap().as_str(),
        )
        .unwrap();
    h.state
        .serving
        .meta
        .put_metadata(missing_blob.as_str(), missing_metadata.as_str())
        .unwrap();
    put_cached_package(
        &h.state.serving,
        "pypi/metadata-skips",
        "pypi",
        "metadata-skips",
        &metadata_skips_project(&invalid_utf8, &missing_blob, &invalid_hex),
    );
    put_cached_package(
        &h.state.serving,
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
    assert_eq!(value["results"][0]["display_label"], "metadata-skips");

    let (status, _headers, body) = get(
        &h.state,
        "/pypi/+search?q=quarantined&type=cached&page_size=25",
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["total"], 0);
}

fn metadata_skips_project(invalid_utf8: &Digest, missing_blob: &Digest, invalid_hex: &Digest) -> ProjectDetail {
    ProjectDetail {
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
    }
}
#[tokio::test]
async fn test_search_indexes_a_project_whose_metadata_sibling_is_malformed() {
    let h = placement_harness().await;
    let artifact = Digest::of(b"malformed metadata");
    h.state
        .serving
        .meta
        .put_metadata(
            artifact.as_str(),
            h.state
                .serving
                .blobs
                .put_bytes(b"Name: malformed\nmalformed header\nVersion: 1.0\n")
                .await
                .unwrap()
                .as_str(),
        )
        .unwrap();
    put_cached_package(
        &h.state.serving,
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
    assert_eq!(value["results"][0]["display_label"], "Malformed");
}
#[tokio::test]
async fn test_search_indexes_metadata_field_lists_and_long_text() {
    let h = harness().await;
    put_uploaded_package_with_metadata(
        &h.state.serving,
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
    assert_eq!(value["results"][0]["resource_key"], "longtext");
    assert_eq!(value["results"][0]["summary"], "metadata fields");
}

#[tokio::test]
async fn test_search_indexes_legacy_home_page() {
    let h = harness().await;
    put_uploaded_package_with_metadata(
        &h.state.serving,
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
    assert_eq!(value["results"][0]["resource_key"], "legacy-home");
}

#[tokio::test]
async fn test_search_indexes_import_names_without_markers() {
    let h = harness().await;
    put_uploaded_package_with_metadata(
        &h.state.serving,
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
        assert_eq!(value["results"][0]["resource_key"], "importable");
    }
}
#[tokio::test]
async fn test_search_availability_filter_keeps_locally_available_projects() {
    let h = placement_harness().await;

    put_cached_package(
        &h.state.serving,
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

    put_cached_package(
        &h.state.serving,
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
        .serving
        .meta
        .put_artifact_placement(
            Digest::of(b"local-dist").as_str(),
            &ArtifactPlacement::record(ArtifactSource::Proxy, true),
        )
        .unwrap();
    h.state.serving.bump_search_epoch();

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
                result["resource_key"].as_str().unwrap(),
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
    assert_eq!(local["results"][0]["resource_key"], "local-dist");
    assert_eq!(local["results"][0]["available"], true);
}

#[tokio::test]
async fn test_search_availability_local_includes_hosted_uploads() {
    let h = harness().await;
    put_uploaded_package(&h.state.serving, "HostedPkg", "hosted-pkg", "A hosted upload");

    let (status, _headers, body) = get(
        &h.state,
        "/hosted/+search?q=hosted-pkg&type=uploaded&availability=local&page_size=25",
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["total"], 1);
    assert_eq!(value["results"][0]["resource_key"], "hosted-pkg");
    assert_eq!(value["results"][0]["available"], true);
}

#[tokio::test]
async fn test_search_availability_excludes_an_evicted_hosted_upload() {
    let h = placement_harness().await;
    put_uploaded_package(
        &h.state.serving,
        "EvictedPkg",
        "evicted-pkg",
        "A hosted upload since evicted",
    );

    let digest = Digest::of(b"evicted-pkg-1.0-py3-none-any.whl");
    h.state
        .serving
        .meta
        .put_artifact_placement(
            digest.as_str(),
            &ArtifactPlacement::record(ArtifactSource::Hosted, false),
        )
        .unwrap();
    h.state.serving.bump_search_epoch();

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

async fn assert_search_quarantine_dominates(layers: Vec<usize>) {
    let (_dir, state) = two_cached_virtual_state(layers);
    put_cached_package(
        &state.serving,
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
        &state.serving,
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
