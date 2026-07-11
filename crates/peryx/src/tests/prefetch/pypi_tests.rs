use peryx_ecosystem_pypi::store::CachedIndex;
use peryx_ecosystem_pypi::store::PypiStore as _;
use peryx_storage::blob::{BlobStore, Digest};
use peryx_storage::meta::MetaStore;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use super::*;
use crate::cli::{PrefetchPlanArgs, PrefetchSyncArgs, PrefetchVerifyArgs};
use crate::config::PrefetchMode;

fn wheel_page(server: &MockServer, wheel: &[u8], metadata: &[u8]) -> String {
    to_json(&serde_json::json!({
        "meta": {"api-version": "1.1"},
        "name": "flask",
        "versions": ["1.0"],
        "files": [{
            "filename": "flask-1.0-py3-none-any.whl",
            "url": format!("{}/files/flask-1.0-py3-none-any.whl", server.uri()),
            "hashes": {"sha256": Digest::of(wheel).as_str()},
            "size": wheel.len(),
            "core-metadata": {"sha256": Digest::of(metadata).as_str()},
        }],
    }))
}

fn put_cached_page(data_dir: &Path, key: &str, name: &str, files: Vec<serde_json::Value>) {
    let source = key.split_once('/').map_or(key, |(source, _)| source);
    let project = key.split_once('/').map_or(key, |(_, project)| project);
    MetaStore::open(data_dir.join("peryx.redb"))
        .unwrap()
        .put_cached_page(
            key,
            &CachedIndex {
                etag: None,
                last_serial: None,
                fetched_at_unix: 0,
                content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
                fresh_secs: None,
                body: detail_page(name, files),
            },
            source,
            project,
            name,
            source,
            None,
            None,
            &[],
            &[],
        )
        .unwrap();
}

async fn mount_project(server: &MockServer, wheel: Vec<u8>, metadata: Vec<u8>) {
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            wheel_page(server, &wheel, &metadata).into_bytes(),
            "application/vnd.pypi.simple.v1+json",
        ))
        .expect(1)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/flask-1.0-py3-none-any.whl"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(wheel))
        .expect(1)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/flask-1.0-py3-none-any.whl.metadata"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(metadata))
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn test_mirror_plan_reports_wheel_tag_filter() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            detail_page(
                "flask",
                vec![
                    file_entry(
                        "flask-1.0-cp311-cp311-macosx_14_0_arm64.whl",
                        Digest::of(b"wheel").as_str(),
                        5,
                    ),
                    file_entry("flask-1.0.tar.gz", Digest::of(b"sdist").as_str(), 5),
                ],
            ),
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&server)
        .await;
    let mut options = command_options(dir.path(), vec!["flask".to_owned()]);
    options.python_tags.push("cp312".to_owned());

    let text = run_ok(
        &mirror(&dir, &server),
        &PrefetchCommand::Plan(PrefetchPlanArgs { options }),
    )
    .await;
    assert!(text.contains("flask-1.0-cp311-cp311-macosx_14_0_arm64.whl"));
    assert!(text.contains("\tskipped\twheel tag filtered"));
    assert!(text.contains("file\tpypi\tflask\tflask-1.0.tar.gz"));
}

#[tokio::test]
async fn test_mirror_plan_accepts_matching_wheel_tags() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            detail_page(
                "flask",
                vec![file_entry(
                    "flask-1.0-cp311-cp311-macosx_14_0_arm64.WHL",
                    Digest::of(b"wheel").as_str(),
                    5,
                )],
            ),
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&server)
        .await;
    let mut options = command_options(dir.path(), vec!["flask".to_owned()]);
    options.python_tags.push("cp311".to_owned());
    options.abi_tags.push("cp311".to_owned());
    options.platform_tags.push("macosx_14_0_arm64".to_owned());

    let text = run_ok(
        &mirror(&dir, &server),
        &PrefetchCommand::Plan(PrefetchPlanArgs { options }),
    )
    .await;
    assert!(text.contains("file\tpypi\tflask\tflask-1.0-cp311-cp311-macosx_14_0_arm64.WHL"));
}

#[tokio::test]
async fn test_mirror_plan_reports_selected_files_without_cache_writes() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let wheel = b"wheel".to_vec();
    let metadata = b"metadata".to_vec();
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            wheel_page(&server, &wheel, &metadata).into_bytes(),
            "application/vnd.pypi.simple.v1+json",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let text = run_ok(
        &mirror(&dir, &server),
        &PrefetchCommand::Plan(PrefetchPlanArgs {
            options: command_options(dir.path(), vec!["Flask==1.0".to_owned()]),
        }),
    )
    .await;
    assert!(text.contains("page\tpypi\tflask"));
    assert!(text.contains("file\tpypi\tflask\tflask-1.0-py3-none-any.whl"));
    assert!(text.contains("metadata\tpypi\tflask\tflask-1.0-py3-none-any.whl.metadata"));
    let meta = MetaStore::open_existing(dir.path().join("peryx.redb")).unwrap();
    assert!(meta.get_index("pypi/flask").unwrap().is_none());
}

#[tokio::test]
async fn test_mirror_plan_reports_missing_and_upstream_failures() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
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
    let (text, err) = run_err(
        &mirror(&dir, &server),
        &PrefetchCommand::Plan(PrefetchPlanArgs {
            options: command_options(dir.path(), vec!["missing".to_owned(), "broken".to_owned()]),
        }),
    )
    .await;
    assert!(err.to_string().contains("prefetch plan found"));
    assert!(text.contains("page\tpypi\tmissing\t\t\t\t\tskipped\tproject not found"));
    assert!(text.contains("page\tpypi\tbroken\t\t\t\t\tfailure\tupstream returned 500"));
}

#[tokio::test]
async fn test_mirror_plan_offline_reads_cached_pages() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = mirror_config(dir.path(), "https://example.invalid/simple/");
    let IndexKind::Cached { offline, .. } = &mut config.indexes[0].kind else {
        panic!("expected cached index");
    };
    *offline = true;
    put_cached_page(
        dir.path(),
        "pypi/flask",
        "flask",
        vec![file_entry("flask-1.0.tar.gz", Digest::of(b"sdist").as_str(), 5)],
    );
    let text = run_ok(
        &config,
        &PrefetchCommand::Plan(PrefetchPlanArgs {
            options: command_options(dir.path(), vec!["flask".to_owned()]),
        }),
    )
    .await;
    assert!(text.contains("file\tpypi\tflask\tflask-1.0.tar.gz"));

    let mut options = command_options(dir.path(), Vec::new());
    options.mode = Some(PrefetchMode::All);
    let text = run_ok(&config, &PrefetchCommand::Plan(PrefetchPlanArgs { options })).await;
    assert!(text.contains("page\tpypi\tflask"));
}

#[tokio::test]
async fn test_mirror_sync_downloads_then_reuses_cached_blobs() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let wheel = b"wheel".to_vec();
    let metadata = b"metadata".to_vec();
    let wheel_digest = Digest::of(&wheel);
    let metadata_digest = Digest::of(&metadata);
    mount_project(&server, wheel, metadata).await;
    let config = mirror(&dir, &server);
    let command = PrefetchCommand::Sync(PrefetchSyncArgs {
        options: command_options(dir.path(), vec!["flask".to_owned()]),
    });
    let first = run_ok(&config, &command).await;
    assert!(first.contains("\tdownloaded\t"));

    let second = run_ok(&config, &command).await;
    assert!(second.contains("file\tpypi\tflask\tflask-1.0-py3-none-any.whl"));
    assert!(second.contains("\tcached\t"));
    let blobs = BlobStore::new(dir.path().join("blobs"));
    assert!(blobs.exists(&wheel_digest));
    assert!(blobs.exists(&metadata_digest));
}

#[tokio::test]
async fn test_mirror_sync_downloads_file_without_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let sdist = b"sdist".to_vec();
    let digest = Digest::of(&sdist);
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            detail_page(
                "flask",
                vec![serde_json::json!({
                    "filename": "flask-1.0.tar.gz",
                    "url": format!("{}/files/flask-1.0.tar.gz", server.uri()),
                    "hashes": {"sha256": digest.as_str()},
                    "size": sdist.len(),
                })],
            ),
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/flask-1.0.tar.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(sdist))
        .mount(&server)
        .await;
    let text = run_ok(
        &mirror(&dir, &server),
        &PrefetchCommand::Sync(PrefetchSyncArgs {
            options: command_options(dir.path(), vec!["flask".to_owned()]),
        }),
    )
    .await;
    assert!(text.contains("file\tpypi\tflask\tflask-1.0.tar.gz"));
    assert!(BlobStore::new(dir.path().join("blobs")).exists(&digest));
}

#[tokio::test]
async fn test_mirror_sync_overlay_target_and_metadata_only_skips_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let wheel = b"wheel".to_vec();
    let metadata = b"metadata".to_vec();
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            wheel_page(&server, &wheel, &metadata).into_bytes(),
            "application/vnd.pypi.simple.v1+json",
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/flask-1.0-py3-none-any.whl.metadata"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(metadata.clone()))
        .expect(1)
        .mount(&server)
        .await;
    let mut options = command_options(dir.path(), vec!["flask".to_owned()]);
    options.index = "root/pypi".to_owned();
    options.metadata_only = true;
    let text = run_ok(
        &overlay_config(dir.path(), &format!("{}/simple/", server.uri())),
        &PrefetchCommand::Sync(PrefetchSyncArgs { options }),
    )
    .await;
    assert!(text.contains("metadata\troot/pypi\tflask\tflask-1.0-py3-none-any.whl.metadata"));
    assert!(text.contains("file\troot/pypi\tflask\tflask-1.0-py3-none-any.whl"));
    assert!(text.contains("\tskipped\tmetadata-only"));
    let blobs = BlobStore::new(dir.path().join("blobs"));
    assert!(blobs.exists(&Digest::of(&metadata)));
    assert!(!blobs.exists(&Digest::of(&wheel)));
}

#[tokio::test]
async fn test_mirror_sync_reports_missing_project_and_metadata_failure() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let wheel = b"wheel".to_vec();
    let metadata = b"metadata".to_vec();
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            wheel_page(&server, &wheel, &metadata).into_bytes(),
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/flask-1.0-py3-none-any.whl.metadata"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"wrong".to_vec()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/flask-1.0-py3-none-any.whl"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(wheel))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/simple/missing/"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let (text, err) = run_err(
        &mirror(&dir, &server),
        &PrefetchCommand::Sync(PrefetchSyncArgs {
            options: command_options(dir.path(), vec!["flask".to_owned(), "missing".to_owned()]),
        }),
    )
    .await;
    assert!(err.to_string().contains("prefetch sync found"));
    assert!(text.contains("page\tpypi\tmissing\t\t\t\t\tskipped\tproject not found"));
    assert!(text.contains("metadata\tpypi\tflask\tflask-1.0-py3-none-any.whl.metadata"));
    assert!(text.contains("\tfailure\t"));
}

#[tokio::test]
async fn test_mirror_sync_reports_page_failure_and_skipped_file() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/broken/"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            detail_page(
                "flask",
                vec![file_entry("flask-1.0.tar.gz", Digest::of(b"sdist").as_str(), 5)],
            ),
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&server)
        .await;
    let mut options = command_options(dir.path(), vec!["broken".to_owned(), "flask".to_owned()]);
    options.no_sdists = true;
    let (text, err) = run_err(
        &mirror(&dir, &server),
        &PrefetchCommand::Sync(PrefetchSyncArgs { options }),
    )
    .await;
    assert!(err.to_string().contains("prefetch sync found"));
    assert!(text.contains("page\tpypi\tbroken\t\t\t\t\tfailure\tupstream is unavailable"));
    assert!(text.contains("file\tpypi\tflask\tflask-1.0.tar.gz"));
    assert!(text.contains("\tskipped\tsdists disabled"));
}

#[tokio::test]
async fn test_mirror_sync_mode_and_size_options_override_prefetch() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let wheel = b"wheel".to_vec();
    let metadata = b"metadata".to_vec();
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            detail_page(
                "flask",
                vec![
                    serde_json::json!({
                        "filename": "flask-1.0-py3-none-any.whl",
                        "url": format!("{}/files/flask-1.0-py3-none-any.whl", server.uri()),
                        "hashes": {"sha256": Digest::of(&wheel).as_str()},
                        "size": wheel.len(),
                        "core-metadata": {"sha256": Digest::of(&metadata).as_str()},
                    }),
                    file_entry("flask-2.0.tar.gz", Digest::of(b"large").as_str(), 1_000),
                ],
            ),
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/flask-1.0-py3-none-any.whl.metadata"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(metadata))
        .mount(&server)
        .await;
    let mut options = command_options(dir.path(), vec!["flask".to_owned()]);
    options.mode = Some(PrefetchMode::MetadataOnly);
    options.max_file_size_bytes = Some(10);
    let text = run_ok(
        &mirror(&dir, &server),
        &PrefetchCommand::Sync(PrefetchSyncArgs { options }),
    )
    .await;
    assert!(text.contains("metadata\tpypi\tflask\tflask-1.0-py3-none-any.whl.metadata"));
    assert!(text.contains("file\tpypi\tflask\tflask-1.0-py3-none-any.whl"));
    assert!(text.contains("\tskipped\tmetadata-only"));
    assert!(text.contains("flask-2.0.tar.gz"));
    assert!(text.contains("\tskipped\tsize filtered"));
}

#[tokio::test]
async fn test_mirror_sync_all_reads_project_list() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(
                to_json(&serde_json::json!({
                    "meta": {"api-version": "1.1"},
                    "projects": [{"name": "Flask"}],
                }))
                .into_bytes(),
                "application/vnd.pypi.simple.v1+json",
            ),
        )
        .expect(1)
        .mount(&server)
        .await;
    mount_project(&server, b"wheel".to_vec(), b"metadata".to_vec()).await;
    let mut options = command_options(dir.path(), Vec::new());
    options.mode = Some(PrefetchMode::All);
    let text = run_ok(
        &mirror(&dir, &server),
        &PrefetchCommand::Sync(PrefetchSyncArgs { options }),
    )
    .await;
    assert!(text.contains("page\tpypi\tflask"));
    assert!(text.contains("file\tpypi\tflask\tflask-1.0-py3-none-any.whl"));
}

#[tokio::test]
async fn test_mirror_plan_reads_html_detail_and_reports_version_filter() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let digest = Digest::of(b"sdist");
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(
                format!(
                    r#"<html><body><a href="{}/files/flask-2.0.tar.gz#sha256={}">flask-2.0.tar.gz</a></body></html>"#,
                    server.uri(),
                    digest.as_str()
                )
                .into_bytes(),
                "text/html",
            ),
        )
        .mount(&server)
        .await;
    let text = run_ok(
        &mirror(&dir, &server),
        &PrefetchCommand::Plan(PrefetchPlanArgs {
            options: command_options(dir.path(), vec!["flask[async]==1.0; python_version>'3.10'".to_owned()]),
        }),
    )
    .await;
    assert!(text.contains("flask-2.0.tar.gz"));
    assert!(text.contains("\tskipped\tversion filtered"));
}

#[tokio::test]
async fn test_mirror_sync_reports_digest_failure() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let expected = b"expected".to_vec();
    let metadata = b"metadata".to_vec();
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            wheel_page(&server, &expected, &metadata).into_bytes(),
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/flask-1.0-py3-none-any.whl"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"wrong".to_vec()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/flask-1.0-py3-none-any.whl.metadata"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(metadata))
        .mount(&server)
        .await;
    let (text, err) = run_err(
        &mirror(&dir, &server),
        &PrefetchCommand::Sync(PrefetchSyncArgs {
            options: command_options(dir.path(), vec!["flask".to_owned()]),
        }),
    )
    .await;
    assert!(err.to_string().contains("prefetch sync found"));
    assert!(text.contains("\tfailure\t"));
}

#[tokio::test]
async fn test_mirror_verify_reports_missing_blob() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let wheel = b"wheel".to_vec();
    let metadata = b"metadata".to_vec();
    let wheel_digest = Digest::of(&wheel);
    let metadata_digest = Digest::of(&metadata);
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let body = wheel_page(&server, &wheel, &metadata).into_bytes();
    meta.put_cached_page(
        "pypi/flask",
        &CachedIndex {
            etag: None,
            last_serial: None,
            fetched_at_unix: 0,
            content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
            fresh_secs: None,
            body,
        },
        "pypi",
        "flask",
        "flask",
        "pypi",
        None,
        None,
        &[(
            wheel_digest.as_str().to_owned(),
            format!("{}/files/flask-1.0-py3-none-any.whl", server.uri()),
            Some(wheel.len() as u64),
        )],
        &[(
            wheel_digest.as_str().to_owned(),
            format!("{}/files/flask-1.0-py3-none-any.whl.metadata", server.uri()),
            metadata_digest.as_str().to_owned(),
        )],
    )
    .unwrap();
    drop(meta);
    let (text, err) = run_err(
        &mirror(&dir, &server),
        &PrefetchCommand::Verify(PrefetchVerifyArgs {
            options: command_options(dir.path(), vec!["flask".to_owned()]),
        }),
    )
    .await;
    assert!(err.to_string().contains("prefetch verify found"));
    assert!(text.contains("file\tpypi\tflask\tflask-1.0-py3-none-any.whl"));
    assert!(text.contains("\tmissing\tblob is not cached"));
}

#[tokio::test]
async fn test_mirror_verify_reports_missing_project_and_metadata_blob() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let wheel = b"wheel".to_vec();
    let metadata = b"metadata".to_vec();
    let wheel_digest = Digest::of(&wheel);
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    meta.put_index(
        "pypi/flask",
        &CachedIndex {
            etag: None,
            last_serial: None,
            fetched_at_unix: 0,
            content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
            fresh_secs: None,
            body: wheel_page(&server, &wheel, &metadata).into_bytes(),
        },
    )
    .unwrap();
    drop(meta);
    BlobStore::new(dir.path().join("blobs"))
        .write_verified(&wheel, &wheel_digest)
        .unwrap();
    let (text, err) = run_err(
        &mirror(&dir, &server),
        &PrefetchCommand::Verify(PrefetchVerifyArgs {
            options: command_options(dir.path(), vec!["flask".to_owned(), "ghost".to_owned()]),
        }),
    )
    .await;
    assert!(err.to_string().contains("prefetch verify found"));
    assert!(text.contains("page\tpypi\tghost\t\t\t\t\tmissing\tproject page is not cached"));
    assert!(text.contains("metadata\tpypi\tflask\tflask-1.0-py3-none-any.whl.metadata"));
    assert!(text.contains("\tmissing\tblob is not cached"));
}

#[tokio::test]
async fn test_mirror_verify_reports_corrupt_page_invalid_digest_and_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    meta.put_index(
        "pypi/broken",
        &CachedIndex {
            etag: None,
            last_serial: None,
            fetched_at_unix: 0,
            content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
            fresh_secs: None,
            body: b"{".to_vec(),
        },
    )
    .unwrap();
    meta.put_index(
        "pypi/flask",
        &CachedIndex {
            etag: None,
            last_serial: None,
            fetched_at_unix: 0,
            content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
            fresh_secs: None,
            body: detail_page(
                "flask",
                vec![
                    file_entry("flask-1.0.tar.gz", "bad", 5),
                    file_entry("flask-2.0.tar.gz", Digest::of(b"expected").as_str(), 8),
                    file_entry("flask-1.0-py3-none-any.unknown", Digest::of(b"unknown").as_str(), 7),
                ],
            ),
        },
    )
    .unwrap();
    drop(meta);
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let expected = Digest::of(b"expected");
    blobs.write_verified(b"expected", &expected).unwrap();
    std::fs::write(blobs.path_for(&expected), b"wrong").unwrap();
    let (text, err) = run_err(
        &mirror(&dir, &server),
        &PrefetchCommand::Verify(PrefetchVerifyArgs {
            options: command_options(dir.path(), vec!["broken".to_owned(), "flask".to_owned()]),
        }),
    )
    .await;
    assert!(err.to_string().contains("prefetch verify found"));
    assert!(text.contains("page\tpypi\tbroken"));
    assert!(text.contains("parse cached project broken"));
    assert!(text.contains("file\tpypi\tflask\tflask-1.0.tar.gz\tbad"));
    assert!(text.contains("\tfailure\tinvalid sha256 digest"));
    assert!(text.contains("flask-2.0.tar.gz"));
    assert!(text.contains("\tfailure\tdigest mismatch"));
}

#[cfg(unix)]
#[tokio::test]
async fn test_mirror_verify_reports_blob_read_error() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let digest = Digest::of(b"expected");
    put_cached_page(
        dir.path(),
        "pypi/flask",
        "flask",
        vec![file_entry("flask-1.0.tar.gz", digest.as_str(), 5)],
    );
    let blobs = BlobStore::new(dir.path().join("blobs"));
    blobs.write_verified(b"expected", &digest).unwrap();
    let path = blobs.path_for(&digest);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    let (text, err) = run_err(
        &mirror(&dir, &server),
        &PrefetchCommand::Verify(PrefetchVerifyArgs {
            options: command_options(dir.path(), vec!["flask".to_owned()]),
        }),
    )
    .await;
    assert!(err.to_string().contains("prefetch verify found"));
    assert!(text.contains("file\tpypi\tflask\tflask-1.0.tar.gz"));
    assert!(text.contains("\tfailure\t"));
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

#[tokio::test]
async fn test_mirror_verify_all_uses_cached_project_list() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    put_cached_page(
        dir.path(),
        "pypi/flask",
        "Flask",
        vec![file_entry("flask-1.0.tar.gz", Digest::of(b"sdist").as_str(), 5)],
    );
    BlobStore::new(dir.path().join("blobs"))
        .write_verified(b"sdist", &Digest::of(b"sdist"))
        .unwrap();
    let mut options = command_options(dir.path(), Vec::new());
    options.mode = Some(PrefetchMode::All);
    let text = run_ok(
        &mirror(&dir, &server),
        &PrefetchCommand::Verify(PrefetchVerifyArgs { options }),
    )
    .await;
    assert!(text.contains("summary\tpypi\t\tproblems\t\t\t0\tproblems"));
}
