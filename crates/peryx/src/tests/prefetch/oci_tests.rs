use peryx_storage::blob::Digest;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use super::*;
use crate::cli::{PrefetchPlanArgs, PrefetchSyncArgs, PrefetchVerifyArgs};

const OCI_MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

fn oci_sha(bytes: &[u8]) -> String {
    format!("sha256:{}", Digest::of(bytes).as_str())
}

fn oci_config(data_dir: &Path, upstream: &str) -> Config {
    Config {
        data_dir: data_dir.to_path_buf(),
        indexes: vec![IndexConfig {
            name: "oci".to_owned(),
            route: "oci".to_owned(),
            policy: PolicyConfig::default(),
            pypi_policy: peryx_ecosystem_pypi::policy::PypiPolicyConfig::default(),
            webhooks: Vec::new(),
            ecosystem: peryx_core::Ecosystem::Oci,
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

fn oci_options(data_dir: &Path, images: Vec<String>) -> PrefetchOptions {
    let mut options = command_options(data_dir, Vec::new());
    options.index = "oci".to_owned();
    options.images = images;
    options
}

async fn mount_oci_image(server: &MockServer, config_blob: &[u8], layer_blob: &[u8]) {
    let manifest = format!(
        r#"{{"schemaVersion":2,"mediaType":"{OCI_MANIFEST_TYPE}","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{}"}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"{}"}}]}}"#,
        oci_sha(config_blob),
        oci_sha(layer_blob),
    );
    Mock::given(method("GET"))
        .and(path("/v2/library/app/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(manifest.into_bytes(), OCI_MANIFEST_TYPE))
        .mount(server)
        .await;
    for blob in [config_blob, layer_blob] {
        Mock::given(method("GET"))
            .and(path(format!("/v2/library/app/blobs/{}", oci_sha(blob))))
            .respond_with(ResponseTemplate::new(200).set_body_raw(blob.to_vec(), "application/octet-stream"))
            .mount(server)
            .await;
    }
}

#[tokio::test]
async fn test_oci_mirror_sync_then_verify() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    mount_oci_image(&server, b"{}", b"a-layer").await;
    let config = oci_config(dir.path(), &format!("{}/", server.uri()));
    let sync = PrefetchCommand::Sync(PrefetchSyncArgs {
        options: oci_options(dir.path(), vec!["library/app:latest".to_owned()]),
    });

    let out = run_ok(&config, &sync).await;
    assert!(out.contains("manifest\tlibrary/app\tlatest"));
    assert!(out.contains("\tsynced\t"));
    assert!(out.contains("summary\toci\t"));

    // Everything is cached now, so verify reports no error and exits zero.
    let verify = PrefetchCommand::Verify(PrefetchVerifyArgs {
        options: oci_options(dir.path(), vec!["library/app:latest".to_owned()]),
    });
    let out = run_ok(&config, &verify).await;
    assert!(out.contains("\tcached\t"));
}

#[tokio::test]
async fn test_oci_mirror_plan_lists_images() {
    let dir = tempfile::tempdir().unwrap();
    let config = oci_config(dir.path(), "http://127.0.0.1:1/");
    let plan = PrefetchCommand::Plan(PrefetchPlanArgs {
        options: oci_options(dir.path(), vec!["library/app:latest".to_owned()]),
    });
    let out = run_ok(&config, &plan).await;
    assert!(out.contains("manifest\toci\tlibrary/app:latest"));
    assert!(out.contains("\tselected\t"));
    assert!(out.contains("summary\toci\t\timages\t\t\t1\timages"));
}

#[tokio::test]
async fn test_oci_mirror_plan_uses_configured_images() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = oci_config(dir.path(), "http://127.0.0.1:1/");
    if let IndexKind::Cached { prefetch, .. } = &mut config.indexes[0].kind {
        prefetch.packages = vec!["library/app:1.0".to_owned()];
    }
    // The configured package list seeds the plan, and `--image` adds a one-off on top.
    let plan = PrefetchCommand::Plan(PrefetchPlanArgs {
        options: oci_options(dir.path(), vec!["library/other:2.0".to_owned()]),
    });
    let out = run_ok(&config, &plan).await;
    assert!(out.contains("library/app:1.0"), "{out}");
    assert!(out.contains("library/other:2.0"), "{out}");
    assert!(out.contains("\t2\timages"), "{out}");
}

#[tokio::test]
async fn test_oci_mirror_plan_on_a_non_cached_index_uses_only_cli_images() {
    // A hosted OCI index carries no `[index.prefetch]` package list, so only `--image` references seed
    // the plan, the non-cached branch of the image resolver.
    let dir = tempfile::tempdir().unwrap();
    let mut config = oci_config(dir.path(), "http://127.0.0.1:1/");
    config.indexes[0].kind = IndexKind::Hosted {
        upload_token: None,
        volatile: false,
    };
    let plan = PrefetchCommand::Plan(PrefetchPlanArgs {
        options: oci_options(dir.path(), vec!["library/app:latest".to_owned()]),
    });
    let out = run_ok(&config, &plan).await;
    assert!(out.contains("manifest\toci\tlibrary/app:latest"), "{out}");
    assert!(out.contains("\t1\timages"), "{out}");
}

#[tokio::test]
async fn test_oci_mirror_requires_an_image() {
    let dir = tempfile::tempdir().unwrap();
    let config = oci_config(dir.path(), "http://127.0.0.1:1/");
    let sync = PrefetchCommand::Sync(PrefetchSyncArgs {
        options: oci_options(dir.path(), Vec::new()),
    });
    let (_text, err) = run_err(&config, &sync).await;
    assert!(err.to_string().contains("at least one image"));
}

#[tokio::test]
async fn test_oci_mirror_reports_a_missing_image_as_error() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let config = oci_config(dir.path(), &format!("{}/", server.uri()));
    let sync = PrefetchCommand::Sync(PrefetchSyncArgs {
        options: oci_options(dir.path(), vec!["library/nope:latest".to_owned()]),
    });
    let (_text, err) = run_err(&config, &sync).await;
    assert!(err.to_string().contains("error"));
}

#[tokio::test]
async fn test_oci_mirror_plan_requires_an_image() {
    let dir = tempfile::tempdir().unwrap();
    let config = oci_config(dir.path(), "http://127.0.0.1:1/");
    let plan = PrefetchCommand::Plan(PrefetchPlanArgs {
        options: oci_options(dir.path(), Vec::new()),
    });
    let (_text, err) = run_err(&config, &plan).await;
    assert!(err.to_string().contains("at least one image"));
}
