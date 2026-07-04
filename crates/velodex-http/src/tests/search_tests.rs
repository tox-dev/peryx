use std::collections::BTreeMap;
use std::sync::Arc;

use axum::http::StatusCode;
use velodex_ecosystem_pypi::{CoreMetadata, File, Meta, ProjectDetail, Provenance, Yanked, to_json};
use velodex_storage::blob::{BlobStore, Digest};
use velodex_storage::meta::{CachedIndex, MetaError, MetaScanError, MetaStore};
use velodex_upstream::UpstreamClient;

use super::http_tests::{get, harness, harness_with_policies};
use crate::cache;
use crate::path_safety::local_file_url;
use crate::policy::{Policy, PolicyConfig};
use crate::search::{PackageSearch, PackageSource, SearchError, SourceFilter};
use crate::state::{AppState, Index, IndexKind};
use crate::upload::Uploaded;

#[tokio::test]
async fn test_search_indexes_uploaded_metadata_and_route_scope() {
    let h = harness().await;
    put_uploaded_package(
        &h.state,
        "VelodexPkg",
        "velodexpkg",
        "Fast package cache for Python indexes",
    );

    let (status, _headers, body) = get(
        &h.state,
        "/local/+search?q=package%20cache&type=hosted&page_size=25",
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["total"], 1);
    assert_eq!(value["route"], "local");
    assert_eq!(value["results"][0]["display_name"], "VelodexPkg");
    assert_eq!(value["results"][0]["normalized_name"], "velodexpkg");
    assert_eq!(value["results"][0]["type"], "hosted");
}

#[tokio::test]
async fn test_search_filters_repository_policy_denials() {
    let overlay_policy = policy(|config| {
        config.block_projects = vec!["flask".to_owned()];
    });
    let h = harness_with_policies(true, true, Policy::default(), Policy::default(), overlay_policy).await;
    put_cached_package(
        &h.state,
        "pypi/flask",
        "pypi",
        "flask",
        &ProjectDetail {
            meta: Meta::default(),
            name: "Flask".to_owned(),
            versions: vec!["1.0".to_owned()],
            files: vec![file_with_hash("flask-1.0-py3-none-any.whl", &"a".repeat(64), None)],
        },
    );

    let (status, _headers, body) = get(
        &h.state,
        "/root/pypi/+search?q=flask&page_size=25",
        Some("application/json"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["total"], 0);
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
        "/pypi/+search?q=direct-mirror&type=upstream&page_size=25",
        Some("application/json"),
    )
    .await;
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["total"], 1);
    assert_eq!(value["results"][0]["display_name"], "DirectMirror");

    let (status, _headers, body) = get(
        &h.state,
        "/local/+search?q=local-only&type=hosted&page_size=25",
        Some("application/json"),
    )
    .await;
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["total"], 1);
    assert_eq!(value["results"][0]["display_name"], "LocalOnly");
}

#[tokio::test]
async fn test_search_handles_empty_queries_and_fallback_params() {
    let h = harness().await;

    let (status, _headers, body) = get(&h.state, "/+search", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({
            "query": "",
            "type": "all",
            "page": 1,
            "page_size": 25,
            "total": 0,
            "results": [],
        })
    );

    let (status, _headers, body) = get(
        &h.state,
        "/+search?q=re:&page=0&page_size=7&ignored=1",
        Some("application/json"),
    )
    .await;
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["page"], 1);
    assert_eq!(value["page_size"], 25);
}

#[tokio::test]
async fn test_search_reports_invalid_type_filters() {
    let h = harness().await;
    for uri in ["/+search?type=blocked", "/local/+search?type=blocked"] {
        let (status, _headers, body) = get(&h.state, uri, Some("application/json")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("invalid package source type"));
    }
}

#[tokio::test]
async fn test_search_reports_invalid_regex() {
    let h = harness().await;
    let (status, _headers, body) = get(&h.state, "/+search?q=re:(broken&page_size=25", Some("application/json")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("RegexQueryError"));
}

#[tokio::test]
async fn test_search_reports_cached_detail_parse_errors() {
    let h = harness().await;
    h.state
        .meta
        .put_index(
            "pypi/broken",
            &cached_index("{\"meta\":{\"api-version\":\"1.1\"},\"files\":"),
        )
        .unwrap();
    h.state.bump_epoch();

    let (status, _headers, body) = get(&h.state, "/+search?q=broken&page_size=25", Some("application/json")).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body.contains("EOF while parsing"));
}

#[tokio::test]
async fn test_search_matches_single_character_literal_queries() {
    let h = harness().await;
    put_uploaded_package(&h.state, "Velodex.Core", "velodex-core", "literal dot package");

    let (status, _headers, body) = get(&h.state, "/local/+search?q=.&page_size=25", Some("application/json")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["total"], 1);
}

#[tokio::test]
async fn test_search_rebuilds_after_delete() {
    let h = harness().await;
    put_uploaded_package(&h.state, "VelodexPkg", "velodexpkg", "Temporary upload");
    let (status, _headers, body) = get(
        &h.state,
        "/local/+search?q=temporary&page_size=25",
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["total"], 1);

    h.state
        .meta
        .delete_upload("local", "velodexpkg", "velodexpkg-1.0-py3-none-any.whl")
        .unwrap();
    h.state.bump_epoch();
    let (status, _headers, body) = get(
        &h.state,
        "/local/+search?q=temporary&page_size=25",
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["total"], 0);
}

#[tokio::test]
async fn test_search_uses_cached_epoch_until_mutation() {
    let h = harness().await;
    put_uploaded_package(&h.state, "VelodexPkg", "velodexpkg", "Temporary upload");

    let (status, _headers, body) = get(
        &h.state,
        "/local/+search?q=temporary&page_size=25",
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["total"], 1);

    let (status, _headers, body) = get(
        &h.state,
        "/local/+search?q=temporary&page_size=25",
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["total"], 1);
}

#[tokio::test]
async fn test_search_rebuilds_after_yank_and_hide_overrides() {
    let h = harness().await;
    cache::persist_page(
        &h.state,
        "pypi/flask",
        "pypi",
        "flask",
        &cached_index(&to_json(&ProjectDetail {
            meta: Meta::default(),
            name: "Flask".to_owned(),
            versions: vec!["1.0".to_owned()],
            files: vec![File {
                filename: "flask-1.0-py3-none-any.whl".to_owned(),
                url: "https://files.example/flask-1.0-py3-none-any.whl".to_owned(),
                hashes: BTreeMap::from([("sha256".to_owned(), "a".repeat(64))]),
                requires_python: None,
                size: Some(10),
                upload_time: None,
                yanked: Yanked::No,
                core_metadata: CoreMetadata::Absent,
                dist_info_metadata: CoreMetadata::Absent,
                gpg_sig: None,
                provenance: Provenance::Absent,
            }],
        })),
    )
    .unwrap();

    cache::set_yanked(&h.state, h.state.index_at(2), "local", "flask", None, Yanked::Yes)
        .await
        .unwrap();
    let (status, _headers, body) = get(
        &h.state,
        "/root/pypi/+search?q=flask&type=upstream-overrides&page_size=25",
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["total"], 1);
    assert_eq!(value["results"][0]["type"], "upstream-overrides");

    cache::remove_files(&h.state, h.state.index_at(2), "local", true, "flask", None)
        .await
        .unwrap();
    let (status, _headers, body) = get(
        &h.state,
        "/root/pypi/+search?q=flask&type=upstream-overrides&page_size=25",
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["total"], 0);
}

#[tokio::test]
async fn test_search_classifies_overlay_upstream_without_overrides() {
    let h = harness().await;
    put_cached_package(
        &h.state,
        "pypi/statused",
        "pypi",
        "statused",
        &ProjectDetail {
            meta: meta_status("archived", "read-only"),
            name: "Statused".to_owned(),
            versions: vec!["1.0".to_owned()],
            files: vec![file_with_hash(
                "statused-1.0-py3-none-any.whl",
                Digest::of(b"statused").as_str(),
                None,
            )],
        },
    );

    let (status, _headers, body) = get(
        &h.state,
        "/root/pypi/+search?q=statused&type=upstream&page_size=25",
        Some("application/json"),
    )
    .await;
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["total"], 1);
    assert_eq!(value["results"][0]["type"], "upstream");
}

#[tokio::test]
async fn test_search_classifies_overlay_without_upload_as_upstream() {
    let (_dir, state) = overlay_state_without_upload();
    put_cached_package(
        &state,
        "pypi/no-upload",
        "pypi",
        "no-upload",
        &ProjectDetail {
            meta: Meta::default(),
            name: "NoUpload".to_owned(),
            versions: vec!["1.0".to_owned()],
            files: vec![file_with_hash(
                "no-upload-1.0-py3-none-any.whl",
                Digest::of(b"no-upload").as_str(),
                None,
            )],
        },
    );

    let (status, _headers, body) = get(
        &state,
        "/root/pypi/+search?q=no-upload&type=upstream&page_size=25",
        Some("application/json"),
    )
    .await;
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["total"], 1);
    assert_eq!(value["results"][0]["type"], "upstream");
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
        "/pypi/+search?q=metadata-skips&type=upstream&page_size=25",
        Some("application/json"),
    )
    .await;
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["total"], 1);
    assert_eq!(value["results"][0]["display_name"], "metadata-skips");

    let (status, _headers, body) = get(
        &h.state,
        "/pypi/+search?q=quarantined&type=upstream&page_size=25",
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
        "/local/+search?q=docs.example&page_size=25",
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
            SourceFilter::Hosted.as_str(),
            SourceFilter::Upstream.as_str(),
            SourceFilter::UpstreamOverrides.as_str(),
        ],
        ["all", "hosted", "upstream", "upstream-overrides"]
    );
    assert_eq!(SourceFilter::from_value("blocked"), None);
    assert_eq!(
        [
            PackageSource::Hosted.label(),
            PackageSource::Upstream.label(),
            PackageSource::UpstreamOverrides.label(),
        ],
        ["Hosted", "Upstream", "Upstream+"]
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

fn policy(configure: impl FnOnce(&mut PolicyConfig)) -> Policy {
    let mut config = PolicyConfig::default();
    configure(&mut config);
    Policy::compile(&config).unwrap()
}

fn put_uploaded_package(state: &crate::state::AppState, display: &str, normalized: &str, summary: &str) {
    put_uploaded_package_with_metadata(
        state,
        normalized,
        &format!("Metadata-Version: 2.1\nName: {display}\nVersion: 1.0\nSummary: {summary}\n"),
        None,
    );
    state.meta.put_project("local", normalized, display).unwrap();
}

fn put_uploaded_package_with_metadata(
    state: &crate::state::AppState,
    normalized: &str,
    metadata: &str,
    requires_python: Option<&str>,
) {
    let filename = format!("{normalized}-1.0-py3-none-any.whl");
    let artifact_digest = Digest::of(filename.as_bytes());
    let metadata_digest = state.blobs.write(metadata.as_bytes()).unwrap();
    state
        .meta
        .put_metadata(artifact_digest.as_str(), "uploaded", metadata_digest.as_str(), "local")
        .unwrap();
    let uploaded = Uploaded {
        version: "1.0".to_owned(),
        file: File {
            filename: filename.clone(),
            url: local_file_url("local", artifact_digest.as_str(), &filename),
            hashes: BTreeMap::from([("sha256".to_owned(), artifact_digest.as_str().to_owned())]),
            requires_python: requires_python.map(str::to_owned),
            size: Some(10),
            upload_time: None,
            yanked: Yanked::No,
            core_metadata: CoreMetadata::Hashes(BTreeMap::from([(
                "sha256".to_owned(),
                metadata_digest.as_str().to_owned(),
            )])),
            dist_info_metadata: CoreMetadata::Hashes(BTreeMap::from([(
                "sha256".to_owned(),
                metadata_digest.as_str().to_owned(),
            )])),
            gpg_sig: None,
            provenance: Provenance::Absent,
        },
    };
    state
        .meta
        .put_upload("local", normalized, &filename, to_json(&uploaded).as_bytes())
        .unwrap();
    state.meta.put_project("local", normalized, normalized).unwrap();
    state.bump_epoch();
}

fn put_cached_package(
    state: &crate::state::AppState,
    key: &str,
    index: &str,
    normalized: &str,
    detail: &ProjectDetail,
) {
    cache::persist_page(state, key, index, normalized, &cached_index(&to_json(detail))).unwrap();
}

fn overlay_state_without_upload() -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let indexes = vec![
        Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            kind: IndexKind::Mirror {
                client: UpstreamClient::new("https://example.test/simple/").unwrap(),
                offline: false,
            },
            policy: Policy::default(),
        },
        Index {
            name: "root/pypi".to_owned(),
            route: "root/pypi".to_owned(),
            policy: Policy::default(),
            kind: IndexKind::Overlay {
                layers: vec![0],
                upload: None,
            },
        },
    ];
    (dir, Arc::new(AppState::new(meta, blobs, 60, indexes)))
}

fn file_with_hash(filename: &str, sha256: &str, requires_python: Option<&str>) -> File {
    File {
        filename: filename.to_owned(),
        url: format!("https://files.example/{filename}"),
        hashes: BTreeMap::from([("sha256".to_owned(), sha256.to_owned())]),
        requires_python: requires_python.map(str::to_owned),
        size: Some(10),
        upload_time: None,
        yanked: Yanked::No,
        core_metadata: CoreMetadata::Absent,
        dist_info_metadata: CoreMetadata::Absent,
        gpg_sig: None,
        provenance: Provenance::Absent,
    }
}

fn meta_status(status: &str, reason: &str) -> Meta {
    Meta {
        project_status: Some(status.to_owned()),
        project_status_reason: Some(reason.to_owned()),
        ..Meta::default()
    }
}

fn cached_index(body: &str) -> CachedIndex {
    CachedIndex {
        etag: None,
        last_serial: None,
        fetched_at_unix: 1000,
        content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
        fresh_secs: Some(60),
        body: body.as_bytes().to_vec(),
    }
}

fn meta_error() -> MetaError {
    MetaError::Decode(serde_json::from_str::<serde_json::Value>("not json").unwrap_err())
}
