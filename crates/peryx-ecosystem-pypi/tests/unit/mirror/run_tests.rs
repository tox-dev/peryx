use std::collections::{BTreeMap, BTreeSet};

use peryx_driver::serving::{MirrorAction, MirrorDriver as _, MirrorRequest};
use peryx_storage::blob::Digest;
use rstest::rstest;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{
    cached_detail, parse_response_detail, pypi_plan, pypi_sync, pypi_verify, raw_detail, sync_file, sync_files,
    sync_metadata, verify_blob,
};
use crate::mirror::test_support::{self, cached_index};
use crate::mirror::{
    ArtifactFilters, BlobCheck, PrefetchConfig, PrefetchMode, PrefetchOptions, ProjectRule, Selection, SyncSummary,
};
use crate::store::{CachedIndex, PypiStore as _};
use crate::{CoreMetadata, File, Meta, ProjectDetail, Provenance, SimpleResponse, Yanked, to_json};

fn config(mode: PrefetchMode, packages: &[&str]) -> PrefetchConfig {
    PrefetchConfig {
        mode,
        packages: packages.iter().map(|value| (*value).to_owned()).collect(),
        requirements: Vec::new(),
        include_wheels: true,
        include_sdists: true,
        python_tags: Vec::new(),
        abi_tags: Vec::new(),
        platform_tags: Vec::new(),
        max_file_size_bytes: None,
        metadata_only: false,
    }
}

fn options() -> PrefetchOptions {
    PrefetchOptions {
        packages: Vec::new(),
        requirements: Vec::new(),
        mode: None,
        metadata_only: false,
        no_wheels: false,
        no_sdists: false,
        python_tags: Vec::new(),
        abi_tags: Vec::new(),
        platform_tags: Vec::new(),
        max_file_size_bytes: None,
    }
}

fn detail(name: &str) -> ProjectDetail {
    ProjectDetail {
        meta: Meta::default(),
        name: name.to_owned(),
        versions: Vec::new(),
        files: Vec::new(),
    }
}

fn artifact_detail(base: &str) -> ProjectDetail {
    ProjectDetail {
        meta: Meta::default(),
        name: "demo".to_owned(),
        versions: vec!["1.0".to_owned()],
        files: vec![
            File {
                filename: "demo-1.0-py3-none-any.whl".to_owned(),
                url: format!("{base}/demo.whl"),
                hashes: BTreeMap::from([("sha256".to_owned(), Digest::of(b"artifact").as_str().to_owned())]),
                requires_python: None,
                size: Some(8),
                upload_time: None,
                yanked: Yanked::No,
                core_metadata: CoreMetadata::Hashes(BTreeMap::from([(
                    "sha256".to_owned(),
                    Digest::of(b"metadata").as_str().to_owned(),
                )])),
                dist_info_metadata: CoreMetadata::Absent,
                gpg_sig: None,
                provenance: Provenance::Absent,
            },
            File {
                filename: "demo-1.0.tar.gz".to_owned(),
                url: format!("{base}/demo.tar.gz"),
                hashes: BTreeMap::new(),
                requires_python: None,
                size: Some(1),
                upload_time: None,
                yanked: Yanked::No,
                core_metadata: CoreMetadata::Absent,
                dist_info_metadata: CoreMetadata::Absent,
                gpg_sig: None,
                provenance: Provenance::Absent,
            },
            File {
                filename: "demo-1.0.zip".to_owned(),
                url: format!("{base}/demo.zip"),
                hashes: BTreeMap::from([("sha256".to_owned(), Digest::of(b"sdist").as_str().to_owned())]),
                requires_python: None,
                size: Some(5),
                upload_time: None,
                yanked: Yanked::No,
                core_metadata: CoreMetadata::Absent,
                dist_info_metadata: CoreMetadata::Absent,
                gpg_sig: None,
                provenance: Provenance::Absent,
            },
        ],
    }
}

fn record(body: Vec<u8>) -> CachedIndex {
    CachedIndex {
        etag: None,
        last_serial: None,
        fetched_at_unix: 0,
        content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
        fresh_secs: None,
        body,
    }
}

#[test]
fn detail_parsers_accept_json_and_html_and_reject_invalid_cache() {
    let json = to_json(&detail("demo")).into_bytes();
    let response = SimpleResponse {
        status: 200,
        source: None,
        url: "https://example.test/simple/demo/".parse().unwrap(),
        content_type: Some("application/json".to_owned()),
        body: json.clone().into(),
        etag: None,
        last_modified: None,
        retry_after: None,
        last_serial: None,
        max_age: None,
    };
    assert_eq!(parse_response_detail("demo", &response).unwrap().name, "demo");
    let html = SimpleResponse {
        content_type: Some("text/html".to_owned()),
        body: b"<!doctype html><html><body></body></html>".to_vec().into(),
        ..response
    };
    assert_eq!(parse_response_detail("demo", &html).unwrap().name, "demo");
    assert_eq!(raw_detail("demo", &record(json)).unwrap().name, "demo");
    assert!(raw_detail("demo", &record(b"bad".to_vec())).is_err());
}

#[tokio::test]
async fn plan_reports_selected_missing_and_failed_projects() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/demo/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(to_json(&detail("demo")), "application/vnd.pypi.simple.v1+json"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/simple/missing/"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/simple/broken/"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let fixture = test_support::state(vec![cached_index(&format!("{}/simple/", server.uri()), false)]);
    let mut output = Vec::new();

    let error = pypi_plan(
        &config(PrefetchMode::Selected, &["demo", "missing", "broken"]),
        &fixture.state,
        "pypi",
        &options(),
        &mut output,
    )
    .await
    .unwrap_err();

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("page\tpypi\tdemo\t\t\t\t\tselected"), "{output}");
    assert_eq!(error.to_string(), "prefetch plan found 1 failure(s)");
    assert!(output.contains("project not found"));
    assert!(output.contains("upstream returned 500"));
}

#[tokio::test]
async fn plan_reports_included_metadata_and_filtered_files() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/demo/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            to_json(&artifact_detail(&server.uri())),
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&server)
        .await;
    let fixture = test_support::state(vec![cached_index(&format!("{}/simple/", server.uri()), false)]);
    let mut output = Vec::new();

    pypi_plan(
        &config(PrefetchMode::Selected, &["demo"]),
        &fixture.state,
        "pypi",
        &options(),
        &mut output,
    )
    .await
    .unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("file\tpypi\tdemo\tdemo-1.0-py3-none-any.whl"));
    assert!(output.contains("metadata\tpypi\tdemo\tdemo-1.0-py3-none-any.whl.metadata"));
    assert!(output.contains("missing sha256"));
}

#[tokio::test]
async fn sync_reports_a_missing_project_without_failure() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/missing/"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let fixture = test_support::state(vec![cached_index(&format!("{}/simple/", server.uri()), false)]);
    let mut output = Vec::new();

    pypi_sync(
        &config(PrefetchMode::Selected, &["missing"]),
        &fixture.state,
        "pypi",
        &options(),
        &mut output,
    )
    .await
    .unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("project not found"));
    assert!(output.contains("packages_seen\t\t\t1\tpackages_seen"));
}

#[tokio::test]
async fn sync_reports_materialization_failures() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/demo/"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let fixture = test_support::state(vec![cached_index(&format!("{}/simple/", server.uri()), false)]);
    let mut output = Vec::new();

    let error = pypi_sync(
        &config(PrefetchMode::Selected, &["demo"]),
        &fixture.state,
        "pypi",
        &options(),
        &mut output,
    )
    .await
    .unwrap_err();

    assert_eq!(error.to_string(), "prefetch sync found 1 failure(s)");
    assert!(String::from_utf8(output).unwrap().contains("failure"));
}

#[tokio::test]
async fn sync_downloads_metadata_and_artifacts() {
    let server = MockServer::start().await;
    mount_artifact_upstream(&server, 1).await;
    let fixture = test_support::state(vec![cached_index(&format!("{}/simple/", server.uri()), false)]);
    let mut output = Vec::new();

    pypi_sync(
        &config(PrefetchMode::Selected, &["demo"]),
        &fixture.state,
        "pypi",
        &options(),
        &mut output,
    )
    .await
    .unwrap();

    let output = String::from_utf8(output).unwrap();
    assert_eq!(
        reported_sizes(&output, "downloaded"),
        [
            ("metadata", "demo-1.0-py3-none-any.whl.metadata", "8"),
            ("file", "demo-1.0-py3-none-any.whl", "8"),
            ("file", "demo-1.0.zip", "5"),
        ]
    );
    assert!(output.contains("files_downloaded\t\t\t3"));
    assert!(output.contains("bytes_downloaded\t\t\t21"));
    server.verify().await;
}

#[tokio::test]
async fn mirror_sync_reports_blob_head_errors_for_artifacts_and_metadata() {
    let server = MockServer::start().await;
    let mut detail = artifact_detail(&server.uri());
    detail.files.truncate(1);
    Mock::given(method("GET"))
        .and(path("/simple/demo/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(to_json(&detail), "application/vnd.pypi.simple.v1+json"))
        .expect(1)
        .mount(&server)
        .await;
    let fixture = test_support::state(vec![cached_index(&format!("{}/simple/", server.uri()), false)]);
    let store = fixture.state.serving.blobs.filesystem_store().unwrap();
    for digest in [Digest::of(b"artifact"), Digest::of(b"metadata")] {
        let path = store.path_for(&digest);
        std::fs::create_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
        std::fs::write(path.parent().unwrap(), b"not a directory").unwrap();
    }
    let configured = toml::Table::from_iter([
        ("mode".to_owned(), toml::Value::String("selected".to_owned())),
        (
            "packages".to_owned(),
            toml::Value::Array(vec![toml::Value::String("demo".to_owned())]),
        ),
    ]);
    let mut output = Vec::new();

    let error = crate::PypiServing
        .mirror(
            fixture.state,
            MirrorRequest {
                action: MirrorAction::Sync,
                index: "pypi",
                settings: &toml::Table::new(),
                configured: &configured,
                overrides: &toml::Table::new(),
            },
            &mut output,
        )
        .await
        .unwrap_err();

    assert_eq!(error, "prefetch sync found 2 failure(s)");
    let output = String::from_utf8(output).unwrap();
    assert_eq!(
        output
            .lines()
            .filter_map(|line| {
                let cells = line.split('\t').collect::<Vec<_>>();
                (cells.get(7) == Some(&"failure"))
                    .then(|| (cells[0], cells[3], cells[6], cells[8].starts_with("blob store error:")))
            })
            .collect::<Vec<_>>(),
        [
            ("metadata", "demo-1.0-py3-none-any.whl.metadata", "", true),
            ("file", "demo-1.0-py3-none-any.whl", "8", true),
        ]
    );
    assert!(output.contains("files_downloaded\t\t\t0"));
    assert!(output.contains("bytes_downloaded\t\t\t0"));
    assert!(output.contains("failures\t\t\t2"));
    server.verify().await;
}

#[rstest]
#[case::file_mode_default("metadata-only", None, None, false)]
#[case::file_mode_false("metadata-only", Some(false), None, false)]
#[case::cli_mode("selected", Some(false), Some("metadata-only"), false)]
#[case::all_metadata("all", Some(true), None, false)]
#[case::all_artifacts("all", Some(false), None, true)]
#[tokio::test]
async fn sync_applies_metadata_only_after_options_merge(
    #[case] configured_mode: &str,
    #[case] configured_metadata_only: Option<bool>,
    #[case] override_mode: Option<&str>,
    #[case] downloads_artifacts: bool,
) {
    let server = MockServer::start().await;
    mount_artifact_upstream(&server, u64::from(downloads_artifacts)).await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"meta":{"api-version":"1.4"},"projects":[{"name":"demo"}]}"#,
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&server)
        .await;
    let mut configured = toml::Table::from_iter([
        ("mode".to_owned(), toml::Value::String(configured_mode.to_owned())),
        (
            "packages".to_owned(),
            toml::Value::Array(vec![toml::Value::String("demo".to_owned())]),
        ),
    ]);
    if let Some(metadata_only) = configured_metadata_only {
        configured.insert("metadata_only".to_owned(), toml::Value::Boolean(metadata_only));
    }
    let mut overrides = toml::Table::new();
    if let Some(mode) = override_mode {
        overrides.insert("mode".to_owned(), toml::Value::String(mode.to_owned()));
    }
    let fixture = test_support::state(vec![cached_index(&format!("{}/simple/", server.uri()), false)]);
    let mut output = Vec::new();

    crate::PypiServing
        .mirror(
            fixture.state,
            MirrorRequest {
                action: MirrorAction::Sync,
                index: "pypi",
                settings: &toml::Table::new(),
                configured: &configured,
                overrides: &overrides,
            },
            &mut output,
        )
        .await
        .unwrap();

    assert_eq!(
        String::from_utf8(output)
            .unwrap()
            .matches("\tskipped\tmetadata-only\n")
            .count(),
        if downloads_artifacts { 0 } else { 2 }
    );
    server.verify().await;
}

async fn mount_artifact_upstream(server: &MockServer, artifact_requests: u64) {
    Mock::given(method("GET"))
        .and(path("/simple/demo/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            to_json(&artifact_detail(&server.uri())),
            "application/vnd.pypi.simple.v1+json",
        ))
        .expect(1)
        .mount(server)
        .await;
    for (path, body, requests) in [
        ("/demo.whl", b"artifact".as_slice(), artifact_requests),
        ("/demo.whl.metadata", b"metadata".as_slice(), 1),
        ("/demo.zip", b"sdist".as_slice(), artifact_requests),
    ] {
        Mock::given(method("GET"))
            .and(wiremock::matchers::path(path))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .expect(requests)
            .mount(server)
            .await;
    }
}

#[tokio::test]
async fn verify_reports_missing_pages_and_invalid_digests() {
    let fixture = test_support::state(vec![cached_index("https://example.test/simple/", true)]);
    fixture
        .state
        .serving
        .meta
        .put_project("pypi", "missing", "missing")
        .unwrap();
    fixture
        .state
        .serving
        .meta
        .put_project("pypi", "broken", "broken")
        .unwrap();
    fixture
        .state
        .serving
        .meta
        .put_index("pypi/broken", &record(b"bad".to_vec()))
        .unwrap();
    let mut output = Vec::new();

    let error = pypi_verify(
        &config(PrefetchMode::All, &[]),
        &fixture.state,
        "pypi",
        &options(),
        &mut output,
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("2 problem"));
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("project page is not cached"));
    assert!(output.contains("parse cached project broken"));
}

#[tokio::test]
async fn verify_accepts_cached_artifacts_and_metadata() {
    let fixture = test_support::state(vec![cached_index("https://example.test/simple/", true)]);
    let detail = artifact_detail("https://example.test");
    fixture.state.serving.meta.put_project("pypi", "demo", "Demo").unwrap();
    fixture
        .state
        .serving
        .meta
        .put_index("pypi/demo", &record(to_json(&detail).into_bytes()))
        .unwrap();
    for bytes in [b"artifact".as_slice(), b"metadata".as_slice(), b"sdist".as_slice()] {
        fixture
            .state
            .serving
            .blobs
            .blocking()
            .put_bytes_as(bytes, &Digest::of(bytes))
            .unwrap();
    }
    let mut output = Vec::new();

    pypi_verify(
        &config(PrefetchMode::All, &[]),
        &fixture.state,
        "pypi",
        &options(),
        &mut output,
    )
    .await
    .unwrap();

    assert!(String::from_utf8(output).unwrap().contains("problems\t\t\t0"));
}

#[tokio::test]
async fn sync_files_reports_cached_metadata_only_and_filtered_files() {
    let fixture = test_support::state(vec![cached_index("https://example.test/simple/", true)]);
    for bytes in [b"artifact".as_slice(), b"metadata".as_slice(), b"sdist".as_slice()] {
        fixture
            .state
            .serving
            .blobs
            .blocking()
            .put_bytes_as(bytes, &Digest::of(bytes))
            .unwrap();
    }
    let configured = config(PrefetchMode::Selected, &[]);
    let target = crate::mirror::selection::target(&configured, &fixture.state.serving, "pypi").unwrap();
    let mut selection = Selection {
        projects: vec!["demo".to_owned()],
        rules: BTreeMap::new(),
        filters: ArtifactFilters {
            include_wheels: true,
            include_sdists: true,
            python_tags: BTreeSet::new(),
            abi_tags: BTreeSet::new(),
            platform_tags: BTreeSet::new(),
            max_file_size_bytes: None,
            metadata_only: false,
        },
    };
    let detail = artifact_detail("https://example.test");
    let mut summary = SyncSummary::default();
    let mut output = Vec::new();
    sync_files(
        &mut output,
        &fixture.state.serving,
        &target,
        "demo",
        &detail,
        &selection,
        &mut summary,
    )
    .await
    .unwrap();
    let output = String::from_utf8(output).unwrap();
    assert_eq!(
        reported_sizes(&output, "cached"),
        [
            ("metadata", "demo-1.0-py3-none-any.whl.metadata", "8"),
            ("file", "demo-1.0-py3-none-any.whl", "8"),
            ("file", "demo-1.0.zip", "5"),
        ]
    );
    assert_eq!(summary.skipped, 1);

    selection.filters.metadata_only = true;
    let mut summary = SyncSummary::default();
    let mut output = Vec::new();
    sync_files(
        &mut output,
        &fixture.state.serving,
        &target,
        "demo",
        &detail,
        &selection,
        &mut summary,
    )
    .await
    .unwrap();
    assert_eq!(summary.skipped, 3);
    assert!(String::from_utf8(output).unwrap().contains("metadata-only"));
}

#[tokio::test]
async fn blob_verification_distinguishes_invalid_missing_and_present() {
    let fixture = test_support::state(Vec::new());
    let present = Digest::of(b"present");
    fixture
        .state
        .serving
        .blobs
        .blocking()
        .put_bytes_as(b"present", &present)
        .unwrap();
    let target = crate::mirror::Target {
        index: "pypi".to_owned(),
        route: "pypi".to_owned(),
        position: 0,
        cached: "pypi".to_owned(),
        client: peryx_upstream::UpstreamClient::new("https://example.test/simple/").unwrap(),
        offline: true,
        prefetch: config(PrefetchMode::Selected, &[]),
    };

    for (digest, expected, status) in [
        ("bad".to_owned(), 1, "invalid sha256 digest"),
        (Digest::of(b"missing").as_str().to_owned(), 1, "blob is not cached"),
        (present.as_str().to_owned(), 0, ""),
    ] {
        let mut output = Vec::new();
        let problems = verify_blob(
            &mut output,
            &fixture.state.serving,
            &target,
            "demo",
            BlobCheck {
                kind: "file",
                filename: "demo.whl",
                digest_hex: &digest,
                url: "https://example.test/demo.whl",
            },
        )
        .await
        .unwrap();
        assert_eq!(problems, expected);
        assert!(String::from_utf8(output).unwrap().contains(status));
    }
}

#[tokio::test]
async fn blob_verification_reports_mismatches_and_backend_errors() {
    let fixture = test_support::state(Vec::new());
    let target = crate::mirror::Target {
        index: "pypi".to_owned(),
        route: "pypi".to_owned(),
        position: 0,
        cached: "pypi".to_owned(),
        client: peryx_upstream::UpstreamClient::new("https://example.test/simple/").unwrap(),
        offline: true,
        prefetch: config(PrefetchMode::Selected, &[]),
    };
    let mismatch = Digest::of(b"expected");
    let store = fixture.state.serving.blobs.filesystem_store().unwrap();
    let mismatch_path = store.path_for(&mismatch);
    std::fs::create_dir_all(mismatch_path.parent().unwrap()).unwrap();
    std::fs::write(&mismatch_path, b"different").unwrap();
    let io_error = Digest::of(b"directory");
    std::fs::create_dir_all(store.path_for(&io_error)).unwrap();

    for (digest, expected) in [(mismatch, "digest mismatch"), (io_error, "failure")] {
        let mut output = Vec::new();
        assert_eq!(
            verify_blob(
                &mut output,
                &fixture.state.serving,
                &target,
                "demo",
                BlobCheck {
                    kind: "file",
                    filename: "demo.whl",
                    digest_hex: digest.as_str(),
                    url: "https://example.test/demo.whl",
                },
            )
            .await
            .unwrap(),
            1
        );
        assert!(String::from_utf8(output).unwrap().contains(expected));
    }
}

#[tokio::test]
async fn offline_plan_reads_cached_pages_and_skips_missing_pages() {
    let fixture = test_support::state(vec![cached_index("https://example.test/simple/", true)]);
    fixture
        .state
        .serving
        .meta
        .put_index("pypi/demo", &record(to_json(&detail("demo")).into_bytes()))
        .unwrap();
    let mut output = Vec::new();
    pypi_plan(
        &config(PrefetchMode::Selected, &["demo", "missing"]),
        &fixture.state,
        "pypi",
        &options(),
        &mut output,
    )
    .await
    .unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("demo"));
    assert!(output.contains("project not found"));
}

#[tokio::test]
async fn cached_detail_and_sync_file_use_the_local_store() {
    let fixture = test_support::state(vec![cached_index("https://example.test/simple/", true)]);
    let configured = config(PrefetchMode::Selected, &[]);
    let target = crate::mirror::selection::target(&configured, &fixture.state.serving, "pypi").unwrap();
    assert!(cached_detail(&fixture.state.serving, &target, "missing").is_err());
    fixture
        .state
        .serving
        .meta
        .put_index("pypi/demo", &record(to_json(&detail("demo")).into_bytes()))
        .unwrap();
    assert_eq!(
        cached_detail(&fixture.state.serving, &target, "demo").unwrap().name,
        "demo"
    );

    let digest = Digest::of(b"cached");
    fixture
        .state
        .serving
        .blobs
        .blocking()
        .put_bytes_as(b"cached", &digest)
        .unwrap();
    let cached = crate::mirror::PrefetchFile {
        filename: "demo-1.0.tar.gz".to_owned(),
        digest: digest.as_str().to_owned(),
        url: "https://example.test/demo-1.0.tar.gz".to_owned(),
        size: Some(6),
        metadata: None,
        source: None,
    };
    assert!(matches!(
        sync_file(fixture.state.serving.clone(), &target, &cached)
            .await
            .unwrap(),
        crate::mirror::SyncOutcome::Cached(6)
    ));
    let invalid = crate::mirror::PrefetchFile {
        digest: "bad".to_owned(),
        ..cached
    };
    assert!(
        sync_file(fixture.state.serving.clone(), &target, &invalid)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn sync_files_reports_invalid_metadata_and_artifact_digests() {
    let fixture = test_support::state(vec![cached_index("https://example.test/simple/", true)]);
    let configured = config(PrefetchMode::Selected, &[]);
    let target = crate::mirror::selection::target(&configured, &fixture.state.serving, "pypi").unwrap();
    let file = File {
        filename: "demo-1.0-py3-none-any.whl".to_owned(),
        url: "https://example.test/demo.whl".to_owned(),
        hashes: BTreeMap::from([("sha256".to_owned(), "bad".to_owned())]),
        requires_python: None,
        size: Some(1),
        upload_time: None,
        yanked: Yanked::No,
        core_metadata: CoreMetadata::Hashes(BTreeMap::from([("sha256".to_owned(), "bad".to_owned())])),
        dist_info_metadata: CoreMetadata::Absent,
        gpg_sig: None,
        provenance: Provenance::Absent,
    };
    let detail = ProjectDetail {
        meta: Meta::default(),
        name: "demo".to_owned(),
        versions: vec!["1.0".to_owned()],
        files: vec![file],
    };
    let selection = Selection {
        projects: vec!["demo".to_owned()],
        rules: BTreeMap::<String, ProjectRule>::new(),
        filters: ArtifactFilters {
            include_wheels: true,
            include_sdists: true,
            python_tags: BTreeSet::new(),
            abi_tags: BTreeSet::new(),
            platform_tags: BTreeSet::new(),
            max_file_size_bytes: None,
            metadata_only: false,
        },
    };
    let mut summary = SyncSummary::default();
    let mut output = Vec::new();

    sync_files(
        &mut output,
        &fixture.state.serving,
        &target,
        "demo",
        &detail,
        &selection,
        &mut summary,
    )
    .await
    .unwrap();

    assert_eq!(summary.failures, 2);
    assert_eq!(String::from_utf8(output).unwrap().matches("\tfailure\t").count(), 2);
    assert!(
        sync_metadata(&fixture.state.serving, "pypi", "demo.metadata", "bad", &"a".repeat(64),)
            .await
            .is_err()
    );
    assert!(
        sync_metadata(&fixture.state.serving, "pypi", "demo.metadata", &"a".repeat(64), "bad",)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn mirror_driver_validates_configuration_for_each_action() {
    let fixture = test_support::state(Vec::new());
    let table = toml::Table::new();
    for action in [MirrorAction::Plan, MirrorAction::Sync, MirrorAction::Verify] {
        let mut output = Vec::new();
        let result = crate::PypiServing
            .mirror(
                fixture.state.clone(),
                MirrorRequest {
                    action,
                    index: "missing",
                    settings: &table,
                    configured: &table,
                    overrides: &table,
                },
                &mut output,
            )
            .await;
        assert!(result.unwrap_err().contains("unknown cached index"));
        assert!(output.is_empty());
    }
}

#[tokio::test]
async fn mirror_driver_reports_a_valid_empty_selection_override() {
    let fixture = test_support::state(vec![cached_index("https://example.invalid/simple/", true)]);
    let table = toml::Table::new();
    let mut output = Vec::new();

    assert_eq!(
        crate::PypiServing
            .mirror(
                fixture.state,
                MirrorRequest {
                    action: MirrorAction::Plan,
                    index: "pypi",
                    settings: &table,
                    configured: &table,
                    overrides: &table,
                },
                &mut output,
            )
            .await
            .unwrap_err(),
        "cached index pypi has no selected packages; add [index.prefetch].packages or --option 'packages=[\"requests\"]'"
    );
    assert!(output.is_empty());
}

fn reported_sizes<'output>(output: &'output str, status: &str) -> Vec<(&'output str, &'output str, &'output str)> {
    output
        .lines()
        .filter_map(|line| {
            let cells = line.split('\t').collect::<Vec<_>>();
            (cells.get(7) == Some(&status)).then(|| (cells[0], cells[3], cells[6]))
        })
        .collect()
}
