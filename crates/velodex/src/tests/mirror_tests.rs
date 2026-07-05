use std::path::Path;

use velodex_ecosystem_pypi::to_json;
use velodex_http::rate_limit::DEFAULT_UPSTREAM_CONCURRENCY;
use velodex_policy::PolicyConfig;
use velodex_storage::blob::{BlobStore, Digest};
use velodex_storage::meta::{CachedIndex, MetaStore};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::cli::{
    PrefetchCommand, PrefetchOptions, PrefetchPlanArgs, PrefetchSyncArgs, PrefetchVerifyArgs, RuntimeArgs,
};
use crate::config::{Config, IndexConfig, IndexKind, PrefetchMode};

fn mirror_config(data_dir: &Path, upstream: &str) -> Config {
    Config {
        data_dir: data_dir.to_path_buf(),
        indexes: vec![IndexConfig {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            policy: PolicyConfig::default(),
            webhooks: Vec::new(),
            ecosystem: velodex_format::Ecosystem::Pypi,
            kind: IndexKind::Cached {
                upstream: upstream.to_owned(),
                username: None,
                password: None,
                token: None,
                upstream_concurrency: DEFAULT_UPSTREAM_CONCURRENCY,
                offline: false,
                prefetch: Box::default(),
            },
        }],
        ..Config::default()
    }
}

fn overlay_config(data_dir: &Path, upstream: &str) -> Config {
    Config {
        data_dir: data_dir.to_path_buf(),
        indexes: vec![
            IndexConfig {
                name: "hosted".to_owned(),
                route: "hosted".to_owned(),
                policy: PolicyConfig::default(),
                webhooks: Vec::new(),
                ecosystem: velodex_format::Ecosystem::Pypi,
                kind: IndexKind::Hosted {
                    upload_token: None,
                    volatile: true,
                },
            },
            IndexConfig {
                name: "pypi".to_owned(),
                route: "pypi".to_owned(),
                policy: PolicyConfig::default(),
                webhooks: Vec::new(),
                ecosystem: velodex_format::Ecosystem::Pypi,
                kind: IndexKind::Cached {
                    upstream: upstream.to_owned(),
                    username: None,
                    password: None,
                    token: None,
                    upstream_concurrency: DEFAULT_UPSTREAM_CONCURRENCY,
                    offline: false,
                    prefetch: Box::default(),
                },
            },
            IndexConfig {
                name: "root/pypi".to_owned(),
                route: "root/pypi".to_owned(),
                policy: PolicyConfig::default(),
                webhooks: Vec::new(),
                ecosystem: velodex_format::Ecosystem::Pypi,
                kind: IndexKind::Virtual {
                    layers: vec!["hosted".to_owned(), "pypi".to_owned()],
                    upload: Some("hosted".to_owned()),
                },
            },
        ],
        ..Config::default()
    }
}

fn command_options(data_dir: &Path, packages: Vec<String>) -> PrefetchOptions {
    PrefetchOptions {
        runtime: RuntimeArgs {
            config: None,
            host: None,
            port: None,
            data_dir: Some(data_dir.to_path_buf()),
            offline: false,
            log_level: None,
            verbose: 0,
            log_format: None,
            log_sink: None,
            log_file: None,
        },
        index: "pypi".to_owned(),
        packages,
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

fn file_entry(filename: &str, digest: impl Into<String>, size: usize) -> serde_json::Value {
    let digest = digest.into();
    serde_json::json!({
        "filename": filename,
        "url": format!("https://files.example/{filename}"),
        "hashes": {"sha256": digest},
        "size": size,
    })
}

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

fn detail_page(name: &str, files: Vec<serde_json::Value>) -> Vec<u8> {
    let files = serde_json::Value::Array(files);
    to_json(&serde_json::json!({
        "meta": {"api-version": "1.1"},
        "name": name,
        "versions": ["1.0", "2.0"],
        "files": files,
    }))
    .into_bytes()
}

fn put_cached_page(data_dir: &Path, key: &str, name: &str, files: Vec<serde_json::Value>) {
    let source = key.split_once('/').map_or(key, |(source, _)| source);
    let project = key.split_once('/').map_or(key, |(_, project)| project);
    MetaStore::open(data_dir.join("velodex.redb"))
        .unwrap()
        .put_mirror_page(
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
async fn test_mirror_plan_expands_nested_requirements_and_trims_options() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    std::fs::write(
        dir.path().join("constraints.txt"),
        "Django==4.2 --hash=sha256:abc\n-r nested.txt\n-r constraints.txt\n# ignored\n--index-url https://example.invalid\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("nested.txt"),
        "flask[async]>=2; python_version>'3.10'\n",
    )
    .unwrap();
    Mock::given(method("GET"))
        .and(path("/simple/django/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            detail_page(
                "django",
                vec![file_entry("django-4.2.tar.gz", Digest::of(b"django").as_str(), 6)],
            ),
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            detail_page(
                "flask",
                vec![file_entry("flask-2.0.tar.gz", Digest::of(b"flask").as_str(), 5)],
            ),
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&server)
        .await;
    let mut options = command_options(dir.path(), Vec::new());
    options.requirements.push(dir.path().join("constraints.txt"));
    let mut out = Vec::new();

    crate::mirror::run(
        &mirror_config(dir.path(), &format!("{}/simple/", server.uri())),
        &PrefetchCommand::Plan(PrefetchPlanArgs { options }),
        &mut out,
    )
    .await
    .unwrap();

    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("page\tpypi\tdjango"));
    assert!(text.contains("page\tpypi\tflask"));
}

#[tokio::test]
async fn test_mirror_plan_rejects_unsupported_selectors() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let errors = [
        "",
        "git+https://example.invalid/pkg @ main",
        "$bad",
        "not valid",
        "pkg=>1",
    ];

    for raw in errors {
        let mut out = Vec::new();
        let err = crate::mirror::run(
            &mirror_config(dir.path(), &format!("{}/simple/", server.uri())),
            &PrefetchCommand::Plan(PrefetchPlanArgs {
                options: command_options(dir.path(), vec![raw.to_owned()]),
            }),
            &mut out,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("parse package selector"), "{raw}: {err}");
    }
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
    let mut out = Vec::new();

    crate::mirror::run(
        &mirror_config(dir.path(), &format!("{}/simple/", server.uri())),
        &PrefetchCommand::Plan(PrefetchPlanArgs { options }),
        &mut out,
    )
    .await
    .unwrap();

    let text = String::from_utf8(out).unwrap();
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
    let mut out = Vec::new();

    crate::mirror::run(
        &mirror_config(dir.path(), &format!("{}/simple/", server.uri())),
        &PrefetchCommand::Plan(PrefetchPlanArgs { options }),
        &mut out,
    )
    .await
    .unwrap();

    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("file\tpypi\tflask\tflask-1.0-cp311-cp311-macosx_14_0_arm64.WHL")
    );
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
    let mut out = Vec::new();

    crate::mirror::run(
        &mirror_config(dir.path(), &format!("{}/simple/", server.uri())),
        &PrefetchCommand::Plan(PrefetchPlanArgs {
            options: command_options(dir.path(), vec!["Flask==1.0".to_owned()]),
        }),
        &mut out,
    )
    .await
    .unwrap();

    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("page\tpypi\tflask"));
    assert!(text.contains("file\tpypi\tflask\tflask-1.0-py3-none-any.whl"));
    assert!(text.contains("metadata\tpypi\tflask\tflask-1.0-py3-none-any.whl.metadata"));
    let meta = MetaStore::open_existing(dir.path().join("velodex.redb")).unwrap();
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
    let mut out = Vec::new();

    let err = crate::mirror::run(
        &mirror_config(dir.path(), &format!("{}/simple/", server.uri())),
        &PrefetchCommand::Plan(PrefetchPlanArgs {
            options: command_options(dir.path(), vec!["missing".to_owned(), "broken".to_owned()]),
        }),
        &mut out,
    )
    .await
    .unwrap_err();

    let text = String::from_utf8(out).unwrap();
    assert!(err.to_string().contains("mirror plan found"));
    assert!(text.contains("page\tpypi\tmissing\t\t\t\t\tskipped\tproject not found"));
    assert!(text.contains("page\tpypi\tbroken\t\t\t\t\tfailure\tupstream returned 500"));
}

#[tokio::test]
async fn test_mirror_plan_offline_reads_cached_pages() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = mirror_config(dir.path(), "https://example.invalid/simple/");
    let IndexKind::Cached { offline, .. } = &mut config.indexes[0].kind else {
        panic!("expected mirror");
    };
    *offline = true;
    put_cached_page(
        dir.path(),
        "pypi/flask",
        "flask",
        vec![file_entry("flask-1.0.tar.gz", Digest::of(b"sdist").as_str(), 5)],
    );
    let mut out = Vec::new();

    crate::mirror::run(
        &config,
        &PrefetchCommand::Plan(PrefetchPlanArgs {
            options: command_options(dir.path(), vec!["flask".to_owned()]),
        }),
        &mut out,
    )
    .await
    .unwrap();

    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("file\tpypi\tflask\tflask-1.0.tar.gz")
    );

    let mut options = command_options(dir.path(), Vec::new());
    options.mode = Some(PrefetchMode::All);
    let mut out = Vec::new();
    crate::mirror::run(&config, &PrefetchCommand::Plan(PrefetchPlanArgs { options }), &mut out)
        .await
        .unwrap();
    assert!(String::from_utf8(out).unwrap().contains("page\tpypi\tflask"));
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
    let config = mirror_config(dir.path(), &format!("{}/simple/", server.uri()));
    let command = PrefetchCommand::Sync(PrefetchSyncArgs {
        options: command_options(dir.path(), vec!["flask".to_owned()]),
    });
    let mut first = Vec::new();
    crate::mirror::run(&config, &command, &mut first).await.unwrap();
    let first = String::from_utf8(first).unwrap();
    assert!(first.contains("\tdownloaded\t"));

    let mut second = Vec::new();
    crate::mirror::run(&config, &command, &mut second).await.unwrap();
    let second = String::from_utf8(second).unwrap();
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
    let mut out = Vec::new();

    crate::mirror::run(
        &mirror_config(dir.path(), &format!("{}/simple/", server.uri())),
        &PrefetchCommand::Sync(PrefetchSyncArgs {
            options: command_options(dir.path(), vec!["flask".to_owned()]),
        }),
        &mut out,
    )
    .await
    .unwrap();

    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("file\tpypi\tflask\tflask-1.0.tar.gz")
    );
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
    let mut out = Vec::new();

    crate::mirror::run(
        &overlay_config(dir.path(), &format!("{}/simple/", server.uri())),
        &PrefetchCommand::Sync(PrefetchSyncArgs { options }),
        &mut out,
    )
    .await
    .unwrap();

    let text = String::from_utf8(out).unwrap();
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
    let mut out = Vec::new();

    let err = crate::mirror::run(
        &mirror_config(dir.path(), &format!("{}/simple/", server.uri())),
        &PrefetchCommand::Sync(PrefetchSyncArgs {
            options: command_options(dir.path(), vec!["flask".to_owned(), "missing".to_owned()]),
        }),
        &mut out,
    )
    .await
    .unwrap_err();

    let text = String::from_utf8(out).unwrap();
    assert!(err.to_string().contains("mirror sync found"));
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
    let mut out = Vec::new();

    let err = crate::mirror::run(
        &mirror_config(dir.path(), &format!("{}/simple/", server.uri())),
        &PrefetchCommand::Sync(PrefetchSyncArgs { options }),
        &mut out,
    )
    .await
    .unwrap_err();

    let text = String::from_utf8(out).unwrap();
    assert!(err.to_string().contains("mirror sync found"));
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
    let mut out = Vec::new();

    crate::mirror::run(
        &mirror_config(dir.path(), &format!("{}/simple/", server.uri())),
        &PrefetchCommand::Sync(PrefetchSyncArgs { options }),
        &mut out,
    )
    .await
    .unwrap();

    let text = String::from_utf8(out).unwrap();
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
    let mut out = Vec::new();

    crate::mirror::run(
        &mirror_config(dir.path(), &format!("{}/simple/", server.uri())),
        &PrefetchCommand::Sync(PrefetchSyncArgs { options }),
        &mut out,
    )
    .await
    .unwrap();

    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("page\tpypi\tflask"));
    assert!(text.contains("file\tpypi\tflask\tflask-1.0-py3-none-any.whl"));
}

#[tokio::test]
async fn test_mirror_sync_all_reads_html_project_list_and_filters_files() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let wheel = b"wheel".to_vec();
    let sdist = b"sdist".to_vec();
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            br#"<html><body><a href="/simple/flask/">Flask</a></body></html>"#.to_vec(),
            "text/html",
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            detail_page(
                "flask",
                vec![
                    file_entry("flask-1.0-py3-none-any.whl", Digest::of(&wheel).as_str(), wheel.len()),
                    file_entry("flask-1.0.tar.gz", Digest::of(&sdist).as_str(), sdist.len()),
                    file_entry("flask-1.0-py3-none-any.unknown", Digest::of(b"unknown").as_str(), 7),
                    serde_json::json!({
                        "filename": "flask-1.0-missing.whl",
                        "url": "https://files.example/flask-1.0-missing.whl",
                        "hashes": {},
                    }),
                ],
            ),
            "application/vnd.pypi.simple.v1+json",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let mut options = command_options(dir.path(), Vec::new());
    options.mode = Some(PrefetchMode::All);
    options.no_wheels = true;
    let mut out = Vec::new();

    crate::mirror::run(
        &mirror_config(dir.path(), &format!("{}/simple/", server.uri())),
        &PrefetchCommand::Plan(PrefetchPlanArgs { options }),
        &mut out,
    )
    .await
    .unwrap();

    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("flask-1.0.tar.gz"));
    assert!(text.contains("flask-1.0-py3-none-any.whl"));
    assert!(text.contains("\tskipped\twheels disabled"));
    assert!(text.contains("\tskipped\tunsupported filename"));
    assert!(text.contains("\tskipped\tmissing sha256"));
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
    let mut out = Vec::new();

    crate::mirror::run(
        &mirror_config(dir.path(), &format!("{}/simple/", server.uri())),
        &PrefetchCommand::Plan(PrefetchPlanArgs {
            options: command_options(dir.path(), vec!["flask[async]==1.0; python_version>'3.10'".to_owned()]),
        }),
        &mut out,
    )
    .await
    .unwrap();

    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("flask-2.0.tar.gz"));
    assert!(text.contains("\tskipped\tversion filtered"));
}

#[tokio::test]
async fn test_mirror_requirements_parse_errors_include_context() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let requirements = dir.path().join("requirements.txt");
    std::fs::write(&requirements, "$bad\n").unwrap();
    let mut options = command_options(dir.path(), Vec::new());
    options.requirements.push(requirements);
    let mut out = Vec::new();

    let err = crate::mirror::run(
        &mirror_config(dir.path(), &format!("{}/simple/", server.uri())),
        &PrefetchCommand::Plan(PrefetchPlanArgs { options }),
        &mut out,
    )
    .await
    .unwrap_err();

    assert!(err.to_string().contains("parse requirement"));
}

#[tokio::test]
async fn test_mirror_all_mode_errors_on_upstream_project_list_status() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let mut options = command_options(dir.path(), Vec::new());
    options.mode = Some(PrefetchMode::All);
    let mut out = Vec::new();

    let err = crate::mirror::run(
        &mirror_config(dir.path(), &format!("{}/simple/", server.uri())),
        &PrefetchCommand::Plan(PrefetchPlanArgs { options }),
        &mut out,
    )
    .await
    .unwrap_err();

    assert!(err.to_string().contains("upstream project list returned 503"));
}

#[tokio::test]
async fn test_mirror_selected_mode_requires_packages() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let mut out = Vec::new();

    let err = crate::mirror::run(
        &mirror_config(dir.path(), &format!("{}/simple/", server.uri())),
        &PrefetchCommand::Plan(PrefetchPlanArgs {
            options: command_options(dir.path(), Vec::new()),
        }),
        &mut out,
    )
    .await
    .unwrap_err();

    assert!(err.to_string().contains("has no selected packages"));
}

#[tokio::test]
async fn test_mirror_rejects_non_mirror_targets() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let mut config = overlay_config(dir.path(), &format!("{}/simple/", server.uri()));
    config.indexes.push(IndexConfig {
        name: "mirror-two".to_owned(),
        route: "mirror-two".to_owned(),
        policy: PolicyConfig::default(),
        webhooks: Vec::new(),
        ecosystem: velodex_format::Ecosystem::Pypi,
        kind: IndexKind::Cached {
            upstream: format!("{}/simple/", server.uri()),
            username: None,
            password: None,
            token: None,
            upstream_concurrency: DEFAULT_UPSTREAM_CONCURRENCY,
            offline: false,
            prefetch: Box::default(),
        },
    });
    config.indexes.push(IndexConfig {
        name: "double".to_owned(),
        route: "double".to_owned(),
        policy: PolicyConfig::default(),
        webhooks: Vec::new(),
        ecosystem: velodex_format::Ecosystem::Pypi,
        kind: IndexKind::Virtual {
            layers: vec!["pypi".to_owned(), "mirror-two".to_owned()],
            upload: None,
        },
    });
    config.indexes.push(IndexConfig {
        name: "local-overlay".to_owned(),
        route: "local-overlay".to_owned(),
        policy: PolicyConfig::default(),
        webhooks: Vec::new(),
        ecosystem: velodex_format::Ecosystem::Pypi,
        kind: IndexKind::Virtual {
            layers: vec!["hosted".to_owned()],
            upload: Some("hosted".to_owned()),
        },
    });
    let commands = [
        ("unknown", "unknown cached index"),
        ("hosted", "is hosted and has no upstream"),
        ("double", "has more than one cached member"),
        ("local-overlay", "has no cached member"),
    ];

    for (repo, expected) in commands {
        let mut options = command_options(dir.path(), vec!["flask".to_owned()]);
        options.index = repo.to_owned();
        let mut out = Vec::new();
        let err = crate::mirror::run(&config, &PrefetchCommand::Plan(PrefetchPlanArgs { options }), &mut out)
            .await
            .unwrap_err();
        assert!(err.to_string().contains(expected), "{repo}: {err}");
    }
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
    let mut out = Vec::new();

    let err = crate::mirror::run(
        &mirror_config(dir.path(), &format!("{}/simple/", server.uri())),
        &PrefetchCommand::Sync(PrefetchSyncArgs {
            options: command_options(dir.path(), vec!["flask".to_owned()]),
        }),
        &mut out,
    )
    .await
    .unwrap_err();

    assert!(err.to_string().contains("mirror sync found"));
    assert!(String::from_utf8(out).unwrap().contains("\tfailure\t"));
}

#[tokio::test]
async fn test_mirror_verify_reports_missing_blob() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let wheel = b"wheel".to_vec();
    let metadata = b"metadata".to_vec();
    let wheel_digest = Digest::of(&wheel);
    let metadata_digest = Digest::of(&metadata);
    let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    let body = wheel_page(&server, &wheel, &metadata).into_bytes();
    meta.put_mirror_page(
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
    let mut out = Vec::new();

    let err = crate::mirror::run(
        &mirror_config(dir.path(), &format!("{}/simple/", server.uri())),
        &PrefetchCommand::Verify(PrefetchVerifyArgs {
            options: command_options(dir.path(), vec!["flask".to_owned()]),
        }),
        &mut out,
    )
    .await
    .unwrap_err();

    let text = String::from_utf8(out).unwrap();
    assert!(err.to_string().contains("mirror verify found"));
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
    let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
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
    let mut out = Vec::new();

    let err = crate::mirror::run(
        &mirror_config(dir.path(), &format!("{}/simple/", server.uri())),
        &PrefetchCommand::Verify(PrefetchVerifyArgs {
            options: command_options(dir.path(), vec!["flask".to_owned(), "ghost".to_owned()]),
        }),
        &mut out,
    )
    .await
    .unwrap_err();

    let text = String::from_utf8(out).unwrap();
    assert!(err.to_string().contains("mirror verify found"));
    assert!(text.contains("page\tpypi\tghost\t\t\t\t\tmissing\tproject page is not cached"));
    assert!(text.contains("metadata\tpypi\tflask\tflask-1.0-py3-none-any.whl.metadata"));
    assert!(text.contains("\tmissing\tblob is not cached"));
}

#[tokio::test]
async fn test_mirror_verify_reports_corrupt_page_invalid_digest_and_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
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
    let mut out = Vec::new();

    let err = crate::mirror::run(
        &mirror_config(dir.path(), &format!("{}/simple/", server.uri())),
        &PrefetchCommand::Verify(PrefetchVerifyArgs {
            options: command_options(dir.path(), vec!["broken".to_owned(), "flask".to_owned()]),
        }),
        &mut out,
    )
    .await
    .unwrap_err();

    let text = String::from_utf8(out).unwrap();
    assert!(err.to_string().contains("mirror verify found"));
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
    let mut out = Vec::new();

    let err = crate::mirror::run(
        &mirror_config(dir.path(), &format!("{}/simple/", server.uri())),
        &PrefetchCommand::Verify(PrefetchVerifyArgs {
            options: command_options(dir.path(), vec!["flask".to_owned()]),
        }),
        &mut out,
    )
    .await
    .unwrap_err();

    let text = String::from_utf8(out).unwrap();
    assert!(err.to_string().contains("mirror verify found"));
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
    let mut out = Vec::new();

    crate::mirror::run(
        &mirror_config(dir.path(), &format!("{}/simple/", server.uri())),
        &PrefetchCommand::Verify(PrefetchVerifyArgs { options }),
        &mut out,
    )
    .await
    .unwrap();

    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("summary\tpypi\t\tproblems\t\t\t0\tproblems")
    );
}
