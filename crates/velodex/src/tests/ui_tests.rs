use std::fmt::Write as _;
use std::io::Write as _;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use http_body_util::BodyExt as _;
use sha2::{Digest as _, Sha256};
use tower::ServiceExt as _;
use velodex_format::pypi::{CoreMetadata, File, Provenance, Yanked, to_json};
use velodex_http::path_safety::local_file_url;
use velodex_http::upload::Uploaded;
use velodex_storage::blob::Digest;

use crate::config::{Config, IndexConfig, IndexKind};
use crate::server::{build_router, build_state, router_for};

fn ui_config(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        indexes: vec![
            IndexConfig {
                name: "pypi".to_owned(),
                route: "pypi".to_owned(),
                policy: velodex_http::policy::PolicyConfig::default(),
                webhooks: Vec::new(),
                kind: IndexKind::Mirror {
                    upstream: "http://127.0.0.1:9/simple/".to_owned(),
                    username: None,
                    password: None,
                    token: None,
                    upstream_concurrency: velodex_http::rate_limit::DEFAULT_UPSTREAM_CONCURRENCY,
                    offline: false,
                    prefetch: Box::default(),
                },
            },
            IndexConfig {
                name: "local".to_owned(),
                route: "local".to_owned(),
                policy: velodex_http::policy::PolicyConfig::default(),
                webhooks: Vec::new(),
                kind: IndexKind::Local {
                    upload_token: Some("s3cret".to_owned()),
                    volatile: true,
                },
            },
            IndexConfig {
                name: "root/pypi".to_owned(),
                route: "root/pypi".to_owned(),
                policy: velodex_http::policy::PolicyConfig::default(),
                webhooks: Vec::new(),
                kind: IndexKind::Overlay {
                    layers: vec!["local".to_owned(), "pypi".to_owned()],
                    upload: Some("local".to_owned()),
                },
            },
        ],
        ..Config::default()
    }
}

fn empty_ui_config(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        indexes: Vec::new(),
        ..Config::default()
    }
}

async fn get(router: &axum::Router, uri: &str) -> (StatusCode, String) {
    let response = router
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn first_files_table(body: &str) -> &str {
    let table = body.find("<table class=\"files\"").unwrap();
    let rest = &body[table..];
    &rest[..rest.find("</table>").unwrap()]
}

/// Upload the frontend fixture wheel through the router, so UI pages have a metadata-rich package.
async fn upload_fixture(router: &axum::Router) {
    let wheel = include_bytes!("../../../../tests/frontend/fixtures/veloxdemo-1.0.0-py3-none-any.whl");
    upload_file(router, "veloxdemo-1.0.0-py3-none-any.whl", wheel).await;
}

async fn upload_file(router: &axum::Router, filename: &str, content: &[u8]) {
    let boundary = "velodexuitest";
    let mut body = Vec::new();
    let filetype = if filename.ends_with(".tar.gz") {
        "sdist"
    } else {
        "bdist_wheel"
    };
    let sha256 = Digest::of(content);
    for (name, value) in [
        (":action", "file_upload"),
        ("name", "veloxdemo"),
        ("version", "1.0.0"),
        ("filetype", filetype),
        ("sha256_digest", sha256.as_str()),
    ] {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"content\"; \
             filename=\"{filename}\"\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(content);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let request = Request::builder()
        .uri("/root/pypi/")
        .method("POST")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header(
            header::AUTHORIZATION,
            format!("Basic {}", STANDARD.encode("__token__:s3cret")),
        )
        .body(Body::from(body))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

fn put_legacy_file(state: &velodex_http::AppState, filename: &str, content: &[u8]) -> Digest {
    let digest = Digest::of(content);
    state.blobs.write_verified(content, &digest).unwrap();
    let uploaded = Uploaded {
        version: "1.0.0".to_owned(),
        file: File {
            filename: filename.to_owned(),
            url: local_file_url("local", digest.as_str(), filename),
            hashes: std::collections::BTreeMap::from([("sha256".to_owned(), digest.as_str().to_owned())]),
            requires_python: None,
            size: Some(content.len() as u64),
            upload_time: None,
            yanked: Yanked::No,
            core_metadata: CoreMetadata::Absent,
            dist_info_metadata: CoreMetadata::Absent,
            gpg_sig: None,
            provenance: Provenance::Absent,
        },
    };
    state
        .meta
        .put_upload("local", "veloxdemo", filename, &to_json(&uploaded).into_bytes())
        .unwrap();
    state.meta.put_project("local", "veloxdemo", "veloxdemo").unwrap();
    digest
}

fn put_filter_files(state: &velodex_http::AppState) {
    put_legacy_file(state, "veloxdemo-1.0.0-cp311-cp311-macosx_14_0_arm64.whl", b"wheel 1");
    put_legacy_file(state, "veloxdemo-1.0.0-cp312-cp312-macosx_14_0_arm64.whl", b"wheel 2");
    put_legacy_file(state, "veloxdemo-1.0.0.tar.gz", b"sdist");
}

#[tokio::test]
async fn test_ui_dashboard_renders_indexes_and_counters() {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&ui_config(&dir)).unwrap();
    let (status, body) = get(&router, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("change serial"));
    assert!(body.contains("root/pypi"));
    assert!(body.contains("badge kind-overlay"));
    assert!(body.contains("badge uploads"));
    // The overlay renders its layers as an ordered stack with the upload target marked.
    assert!(body.contains("layer-stack"));
    assert!(body.contains("uploads land here"));
    assert!(body.contains("resolves top to bottom"));
    assert!(body.contains("/pkg/velodex_web.js"));
}

#[tokio::test]
async fn test_ui_admin_status_renders_read_only_state_without_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&ui_config(&dir)).unwrap();
    let (status, body) = get(&router, "/admin/status").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Admin status"));
    assert!(body.contains("read-only"));
    assert!(body.contains("root/pypi"));
    assert!(body.contains("/root/pypi/simple/"));
    assert!(body.contains("/browse?index=local"));
    assert!(body.contains("Usage and health"));
    assert!(body.contains("Recent uploads"));
    assert!(body.contains("No uploads recorded yet."));
    assert!(body.contains("token configured"));
    assert!(body.contains("redacted"));
    assert!(body.contains("http://127.0.0.1:9/simple/"));
    assert!(body.contains("upload-enabled"));
    assert!(!body.contains("s3cret"));
    assert!(!body.contains("type=\"password\""));
    assert!(!body.contains("delete whole project"));
}

#[tokio::test]
async fn test_ui_admin_status_empty_state() {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&empty_ui_config(&dir)).unwrap();
    let (status, body) = get(&router, "/admin/status").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("No indexes configured."));
    assert!(body.contains("No usage recorded yet."));
    assert!(body.contains("No uploads recorded yet."));
}

#[tokio::test]
async fn test_ui_admin_status_lists_counts_and_recent_uploads() {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&ui_config(&dir)).unwrap();
    upload_fixture(&router).await;
    let (status, body) = get(&router, "/admin/status").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("uploads"));
    assert!(body.contains("veloxdemo"));
    assert!(body.contains("veloxdemo-1.0.0-py3-none-any.whl"));
    assert!(body.contains("1.2 kB"));
    assert!(!body.contains("A demonstration package"));
}

#[tokio::test]
async fn test_ui_browse_lists_projects_after_upload() {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&ui_config(&dir)).unwrap();
    upload_fixture(&router).await;
    let (status, body) = get(&router, "/browse?index=local").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("veloxdemo"));
    assert!(body.contains("Filter projects"));
}

#[tokio::test]
async fn test_ui_browse_empty_index_hint() {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&ui_config(&dir)).unwrap();
    let (status, body) = get(&router, "/browse?index=local").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("No projects observed"));
}

#[tokio::test]
async fn test_ui_project_page_renders_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&ui_config(&dir)).unwrap();
    upload_fixture(&router).await;
    let (status, body) = get(&router, "/browse?index=local&project=veloxdemo").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("A demonstration package for the velox web UI"));
    assert!(body.contains("uv pip install --index-url /local/simple/ veloxdemo"));
    assert!(body.contains("A demo package served by <strong>velox</strong>"));
    assert!(body.contains("requests&gt;=2"));
    assert!(body.contains("Development Status"));
    assert!(body.contains("badge meta-badge"));
    assert!(body.contains("Manage uploads"));
    assert!(body.contains("1.2 kB"));
}

#[tokio::test]
async fn test_ui_project_page_filters_files_by_substring() {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&ui_config(&dir)).unwrap();
    put_filter_files(&state);
    let router = router_for(state);

    let (status, body) = get(&router, "/browse?index=local&project=veloxdemo&filename=cp312").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("1 of 3 files"));
    let table = first_files_table(&body);
    assert!(table.contains("veloxdemo-1.0.0-cp312-cp312-macosx_14_0_arm64.whl"));
    assert!(!table.contains("veloxdemo-1.0.0-cp311-cp311-macosx_14_0_arm64.whl"));
    assert!(!table.contains("veloxdemo-1.0.0.tar.gz"));
}

#[tokio::test]
async fn test_ui_project_page_filters_files_by_regex() {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&ui_config(&dir)).unwrap();
    put_filter_files(&state);
    let router = router_for(state);

    let (status, body) = get(
        &router,
        "/browse?index=local&project=veloxdemo&filename=cp31%5B12%5D.*whl&filename_match=regex",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("2 of 3 files"));
    let table = first_files_table(&body);
    assert!(table.contains("veloxdemo-1.0.0-cp311-cp311-macosx_14_0_arm64.whl"));
    assert!(table.contains("veloxdemo-1.0.0-cp312-cp312-macosx_14_0_arm64.whl"));
    assert!(!table.contains("veloxdemo-1.0.0.tar.gz"));
}

#[tokio::test]
async fn test_ui_project_page_invalid_regex_keeps_full_table() {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&ui_config(&dir)).unwrap();
    put_filter_files(&state);
    let router = router_for(state);

    let (status, body) = get(
        &router,
        "/browse?index=local&project=veloxdemo&filename=%5B&filename_match=regex",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Invalid regex"));
    assert!(body.contains("3 files"));
    let table = first_files_table(&body);
    assert!(table.contains("veloxdemo-1.0.0-cp311-cp311-macosx_14_0_arm64.whl"));
    assert!(table.contains("veloxdemo-1.0.0-cp312-cp312-macosx_14_0_arm64.whl"));
    assert!(table.contains("veloxdemo-1.0.0.tar.gz"));
}

#[tokio::test]
async fn test_ui_project_page_missing_project() {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&ui_config(&dir)).unwrap();
    let (status, body) = get(&router, "/browse?index=local&project=ghost").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Project not found on this index."));
}

#[tokio::test]
async fn test_ui_project_page_shows_contents_for_zipped_eggs() {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&ui_config(&dir)).unwrap();
    let mut egg = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut egg));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("EGG-INFO/PKG-INFO", options).unwrap();
        zip.write_all(b"Metadata-Version: 1.2\nName: veloxdemo\nVersion: 1.0.0\n")
            .unwrap();
        zip.finish().unwrap();
    }
    let digest = put_legacy_file(&state, "veloxdemo-1.0.0.egg", &egg);
    let router = router_for(state);
    let (status, body) = get(&router, "/browse?index=local&project=veloxdemo").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("veloxdemo-1.0.0.egg"));
    assert!(body.contains("class=\"inspect\""));

    let url = format!(
        "/browse?index=local&project=veloxdemo&sha256={}&file=veloxdemo-1.0.0.egg",
        digest.as_str()
    );
    let (status, body) = get(&router, &url).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("EGG-INFO/PKG-INFO"));
}

#[tokio::test]
async fn test_ui_project_page_hides_contents_for_unsupported_legacy_tar() {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&ui_config(&dir)).unwrap();
    put_legacy_file(&state, "veloxdemo-1.0.0.tar.bz2", b"legacy tarball");
    let router = router_for(state);
    let (status, body) = get(&router, "/browse?index=local&project=veloxdemo").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("veloxdemo-1.0.0.tar.bz2"));
    assert!(!body.contains("class=\"inspect\""));
}

#[tokio::test]
async fn test_ui_archive_listing_and_member() {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&ui_config(&dir)).unwrap();
    upload_fixture(&router).await;
    let (_, detail) = get(&router, "/local/simple/veloxdemo/").await;
    let sha = detail
        .split("files/")
        .nth(1)
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .to_owned();

    let file = "veloxdemo-1.0.0-py3-none-any.whl";
    let listing_url = format!("/browse?index=local&project=veloxdemo&sha256={sha}&file={file}");
    let (status, listing) = get(&router, &listing_url).await;
    assert_eq!(status, StatusCode::OK);
    assert!(listing.contains("dist-info/METADATA"));
    assert!(listing.contains("__init__.py"));

    let member = format!("{listing_url}&member=veloxdemo-1.0.0.dist-info%2FMETADATA");
    let (status, content) = get(&router, &member).await;
    assert_eq!(status, StatusCode::OK);
    assert!(content.contains("Metadata-Version: 2.1"));
    assert!(content.contains("back to archive"));
}

#[tokio::test]
async fn test_ui_archive_tree_links_nested_archives_and_blocks_binary_preview() {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&ui_config(&dir)).unwrap();
    let mut inner = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut inner));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("pkg/mod.py", options).unwrap();
        zip.write_all(b"x = 1\n").unwrap();
        zip.finish().unwrap();
    }
    let mut wheel = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut wheel));
        let options = zip::write::SimpleFileOptions::default();
        let dist_info = "veloxdemo-1.0.0.dist-info";
        let entries = vec![
            ("veloxdemo/__init__.py".to_owned(), Vec::new()),
            ("veloxdemo/data.bin".to_owned(), vec![0xff, 0xfe]),
            ("vendor/inner.zip".to_owned(), inner),
            (
                format!("{dist_info}/METADATA"),
                b"Metadata-Version: 2.1\nName: veloxdemo\nVersion: 1.0.0\n".to_vec(),
            ),
            (
                format!("{dist_info}/WHEEL"),
                b"Wheel-Version: 1.0\nGenerator: velodex-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n".to_vec(),
            ),
        ];
        for (path, bytes) in &entries {
            zip.start_file(path, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        let record_path = format!("{dist_info}/RECORD");
        zip.start_file(&record_path, options).unwrap();
        zip.write_all(wheel_record(&entries, &record_path).as_bytes()).unwrap();
        zip.finish().unwrap();
    }
    upload_file(&router, "veloxdemo-1.0.0-py3-none-any.whl", &wheel).await;
    let (_, detail) = get(&router, "/local/simple/veloxdemo/").await;
    let sha = detail
        .split("files/")
        .nth(1)
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .to_owned();

    let file = "veloxdemo-1.0.0-py3-none-any.whl";
    let listing_url = format!("/browse?index=local&project=veloxdemo&sha256={sha}&file={file}");
    let (status, listing) = get(&router, &listing_url).await;
    assert_eq!(status, StatusCode::OK);
    assert!(listing.contains("class=\"archive-tree\""));
    assert!(listing.contains("vendor"));
    assert!(listing.contains("inner.zip"));
    assert!(listing.contains("container=vendor%2Finner.zip"));
    assert!(listing.contains("data.bin"));
    assert!(!listing.contains("member=veloxdemo%2Fdata.bin"));

    let binary_url = format!("{listing_url}&member=veloxdemo%2Fdata.bin");
    let (status, binary) = get(&router, &binary_url).await;
    assert_eq!(status, StatusCode::OK);
    assert!(binary.contains("archive member"));
    assert!(binary.contains("cannot be previewed inline"));

    let nested_url = format!("{listing_url}&container=vendor%2Finner.zip");
    let (status, nested) = get(&router, &nested_url).await;
    assert_eq!(status, StatusCode::OK);
    assert!(nested.contains("pkg"));
    assert!(nested.contains("mod.py"));

    let member_url = format!("{nested_url}&member=pkg%2Fmod.py");
    let (status, content) = get(&router, &member_url).await;
    assert_eq!(status, StatusCode::OK);
    assert!(content.contains("x = 1"));
}

fn wheel_record(entries: &[(String, Vec<u8>)], record_path: &str) -> String {
    let mut record = String::new();
    for (path, bytes) in entries {
        let digest = URL_SAFE_NO_PAD.encode(Sha256::digest(bytes));
        writeln!(record, "{path},sha256={digest},{}", bytes.len()).unwrap();
    }
    writeln!(record, "{record_path},,").unwrap();
    record
}

#[tokio::test]
async fn test_ui_unknown_route_falls_back() {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&ui_config(&dir)).unwrap();
    let (status, body) = get(&router, "/nosuchpage").await;
    // The catch-all API dispatcher answers for non-UI paths.
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("not found"));
}

#[tokio::test]
async fn test_ui_stats_drills_from_index_to_files() {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&ui_config(&dir)).unwrap();
    upload_fixture(&router).await;
    // The aggregator applies the upload event on its own thread; poll the rendered page.
    let mut body = String::new();
    for _ in 0..500 {
        let (status, page) = get(&router, "/stats?index=root%2Fpypi").await;
        assert_eq!(status, StatusCode::OK);
        if page.contains("veloxdemo") {
            body = page;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(body.contains("uploads"));
    // Leptos escapes attribute ampersands in server output.
    assert!(body.contains("/stats?index=root%2Fpypi&amp;project=veloxdemo"));

    let (status, top) = get(&router, "/stats").await;
    assert_eq!(status, StatusCode::OK);
    assert!(top.contains("/stats?index=root%2Fpypi"));

    let (status, files) = get(&router, "/stats?index=root%2Fpypi&project=veloxdemo").await;
    assert_eq!(status, StatusCode::OK);
    assert!(files.contains("rejected downloads"));
}
