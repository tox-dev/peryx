use std::num::{NonZeroU32, NonZeroUsize};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use futures_util::TryStreamExt as _;
use http_body_util::BodyExt as _;
use peryx_driver::IndexKind as RuntimeKind;
use peryx_identity::{GrantScope, ProviderId, Role};
use peryx_storage::meta::{JobKind, JobState, MetaStore, NewJobRun, PolicyDecisionQuery};
use peryx_upstream::Auth;
#[cfg(unix)]
use peryx_upstream::{CredentialFailure, ExecCredentialConfig};
use rstest::rstest;
use tower::ServiceExt as _;
use wiremock::matchers::{header as match_header, header_regex, method, path};
use wiremock::{Match, Mock, MockServer, Request as WiremockRequest, ResponseTemplate};

use crate::config::{
    AuthConfig, AvailabilityConfig, BlobStorageConfig, Config, CredentialFailureMode, CredentialRefreshConfig,
    DcMember, DcMembership, DcRole, IndexConfig, IndexKind, LdapBindConfig, LdapProviderConfig, LogSink,
    OidcProviderConfig, ReplicationConfig, S3StorageConfig, SecretSource, TrustedPublisherConfig, UpstreamConfig,
    UpstreamRoutingConfig, WebhookConfig, WebhookSecret,
};
use crate::server::{
    build_blob_storage, build_index_settings, build_indexes, build_router, build_state, check_config,
    frontier_endpoint_router, receipt_endpoint_router, receipt_sources, recover_job_attempts, remote_frontier_sources,
    router_for, upstream_auth,
};

fn s3_blob_config(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        blob: BlobStorageConfig::S3(S3StorageConfig {
            endpoint: "https://s3.example.com".to_owned(),
            bucket: "cache".to_owned(),
            prefix: String::new(),
            region: "us-east-1".to_owned(),
            path_style: true,
            request_timeout: Duration::from_secs(30),
            max_retries: 3,
            multipart_threshold: 16 << 20,
            part_size: 16 << 20,
            upload_concurrency: 4,
            conditional_writes: true,
            checksum_writes: true,
        }),
        ..Config::default()
    }
}

#[test]
fn test_build_blob_storage_selects_the_filesystem_backend() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    let storage = build_blob_storage(&config).unwrap();
    assert_eq!(storage.name(), "filesystem");
}

#[test]
fn test_build_blob_storage_opens_the_s3_backend() {
    let dir = tempfile::tempdir().unwrap();
    let storage = build_blob_storage(&s3_blob_config(&dir)).unwrap();
    assert_eq!(storage.name(), "s3");
}

#[test]
fn test_plugin_registry_rejects_settings_for_an_uninstalled_ecosystem() {
    let ecosystem = peryx_core::Ecosystem::new("missing");

    let error = peryx_plugin_registry::compile_index_settings(ecosystem, "index", &toml::Table::new()).unwrap_err();

    assert_eq!(error, "ecosystem missing is not installed");
}

#[test]
fn test_plugin_registry_rejects_snippets_for_an_uninstalled_ecosystem() {
    let ecosystem = peryx_core::Ecosystem::new("missing");
    let base = peryx_driver::discovery::BaseUrl::parse("https://packages.example/").unwrap();

    let error = peryx_plugin_registry::snippet_text(ecosystem, &base, "index", false, "text").unwrap_err();

    assert_eq!(error, "ecosystem missing is not installed");
}

fn config_with(dir: &tempfile::TempDir, indexes: Vec<IndexConfig>) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        indexes,
        ..Config::default()
    }
}

fn cached(name: &str, upstream: &str) -> IndexConfig {
    IndexConfig {
        name: name.to_owned(),
        route: name.to_owned(),
        policy: peryx_policy::PolicyConfig::default(),
        ecosystem_policy: toml::Table::new(),
        ecosystem_settings: toml::Table::new(),
        webhooks: Vec::new(),
        ecosystem: peryx_ecosystem_pypi::ECOSYSTEM,
        anonymous_read: None,
        tokens: Vec::new(),
        kind: IndexKind::Cached {
            routing: super::single_route(upstream),
            upstream_concurrency: peryx_driver::rate_limit::DEFAULT_UPSTREAM_CONCURRENCY,
            offline: false,
            prefetch: Box::default(),
        },
    }
}

fn hosted(name: &str) -> IndexConfig {
    IndexConfig {
        name: name.to_owned(),
        route: name.to_owned(),
        policy: peryx_policy::PolicyConfig::default(),
        ecosystem_policy: toml::Table::new(),
        ecosystem_settings: toml::Table::new(),
        webhooks: Vec::new(),
        ecosystem: peryx_ecosystem_pypi::ECOSYSTEM,
        anonymous_read: None,
        tokens: Vec::new(),
        kind: IndexKind::Hosted { volatile: true },
    }
}

fn virtual_index(layers: &[&str], upload: Option<&str>) -> IndexConfig {
    IndexConfig {
        name: "team".to_owned(),
        route: "team/dev".to_owned(),
        policy: peryx_policy::PolicyConfig::default(),
        ecosystem_policy: toml::Table::new(),
        ecosystem_settings: toml::Table::new(),
        webhooks: Vec::new(),
        ecosystem: peryx_ecosystem_pypi::ECOSYSTEM,
        anonymous_read: None,
        tokens: Vec::new(),
        kind: IndexKind::Virtual {
            layers: layers.iter().map(|&name| name.to_owned()).collect(),
            upload: upload.map(str::to_owned),
        },
    }
}

fn claim_writer(dir: &tempfile::TempDir, identity: &str) {
    MetaStore::open(dir.path().join("peryx.redb"))
        .unwrap()
        .claim_writer_identity(identity)
        .unwrap();
}

fn write_netrc(path: &Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

fn current_auth(client: &peryx_upstream::UpstreamClient) -> Auth {
    client.current_credential().unwrap().auth().clone()
}

struct NoAuthorization;

impl Match for NoAuthorization {
    fn matches(&self, request: &WiremockRequest) -> bool {
        !request.headers.contains_key("authorization")
    }
}

fn availability_replica(configured: bool) -> AvailabilityConfig {
    if configured {
        AvailabilityConfig::Dc(ReplicationConfig::Replica {
            upstream: "https://writer.example/".to_owned(),
            token: SecretSource::Literal("secret".to_owned()),
            poll_interval: Duration::from_secs(1),
            page_size: NonZeroUsize::MIN,
        })
    } else {
        AvailabilityConfig::None
    }
}

fn routed(metadata: &str, artifact: Option<&str>) -> IndexConfig {
    let mut index = cached("pypi", "https://primary.example/simple/");
    let IndexKind::Cached { routing, .. } = &mut index.kind else {
        panic!("expected a cached index");
    };
    *routing = UpstreamRoutingConfig {
        upstreams: vec![UpstreamConfig {
            name: "primary".to_owned(),
            url: metadata.to_owned(),
            artifact_url: artifact.map(str::to_owned),
            username: None,
            password: None,
            token: None,
            credential_exec: None,
            credential_refresh: None,
            tls: crate::config::UpstreamTlsConfig::default(),
        }],
        fallback: true,
        protected: Vec::new(),
        pins: std::collections::BTreeMap::new(),
    };
    index
}

#[cfg(unix)]
fn exec_credential_helper(dir: &tempfile::TempDir) -> (ExecCredentialConfig, PathBuf, PathBuf) {
    let executable = dir.path().join("credential-helper");
    let executions = dir.path().join("credential-executions");
    let requests = dir.path().join("credential-requests");
    let quote = |path: &Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
    std::fs::write(
        &executable,
        format!(
            "#!/bin/sh\nset -u\nrequest=$(/bin/cat)\nprintf '%s' \"$request\" > {}\nprintf x >> {}\nprintf '%s' \
             '{{\"version\":1,\"expires_at\":\"2099-01-01T00:00:00Z\",\"type\":\"bearer\",\"token\":\"exec-token\"}}'\n",
            quote(&requests),
            quote(&executions),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    (
        ExecCredentialConfig::new(
            vec![executable.display().to_string()],
            Duration::from_secs(5),
            Vec::new(),
            CredentialFailure::Fail,
        )
        .unwrap(),
        executions,
        requests,
    )
}

#[tokio::test]
async fn test_build_router_serves_status() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    let state = build_state(&config).unwrap();
    let user = state.users.create("Alice").unwrap();
    state.users.set_password(&user.id, "local password").await.unwrap();
    state
        .authorization
        .grant(&user.id, Role::Administrator, GrantScope::Server)
        .unwrap();
    let authorization = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("Alice:local password")
    );
    let router = router_for(state);
    let response = tokio::task::LocalSet::new()
        .run_until(
            router.oneshot(
                Request::builder()
                    .uri("/+status")
                    .header(header::AUTHORIZATION, authorization)
                    .body(Body::empty())
                    .unwrap(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("root/pypi"));
}

#[rstest]
#[case::liveness("/+health", r#"{"status":"live"}"#)]
#[case::readiness("/+ready", r#"{"status":"ready"}"#)]
fn test_build_router_serves_public_probes(#[case] uri: &str, #[case] expected: &str) {
    let dir = tempfile::tempdir().unwrap();
    tokio::task::LocalSet::new().block_on(
        &tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap(),
        async {
            let router = build_router(&Config {
                data_dir: dir.path().to_path_buf(),
                ..Config::default()
            })
            .unwrap();
            let response = router
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(body.as_ref(), expected.as_bytes());
        },
    );
}

#[tokio::test]
async fn test_build_router_fails_over_live_simple_requests() {
    let dir = tempfile::tempdir().unwrap();
    let first = MockServer::start().await;
    let second = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&first)
        .await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            br#"{"meta":{"api-version":"1.1"},"name":"flask","versions":[],"files":[]}"#.to_vec(),
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&second)
        .await;
    let partial = crate::config::from_toml(
        PathBuf::from("x.toml"),
        &format!(
            "[[index]]\nname = \"pypi\"\n\
             [[index.upstream]]\nname = \"first\"\nurl = \"{}/simple/\"\n\
             [[index.upstream]]\nname = \"second\"\nurl = \"{}/simple/\"\n",
            first.uri(),
            second.uri()
        ),
    )
    .unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default().apply(partial).unwrap()
    };
    let router = build_router(&config).unwrap();

    let response = tokio::task::LocalSet::new()
        .run_until(
            router.oneshot(
                Request::builder()
                    .uri("/pypi/simple/flask/")
                    .header("accept", "application/vnd.pypi.simple.v1+json")
                    .body(Body::empty())
                    .unwrap(),
            ),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(String::from_utf8_lossy(&response.into_body().collect().await.unwrap().to_bytes()).contains("flask"));
}

#[test]
fn test_build_state_opens_configured_data_dir() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };

    let state = build_state(&config).unwrap();

    assert_eq!(state.indexes.len(), config.indexes.len());
    assert!(dir.path().join("peryx.redb").exists());
}

#[rstest]
#[case::writer(false, 1, JobState::Failed)]
#[case::read_only(true, 0, JobState::Running)]
fn test_recover_job_attempts_only_updates_writers(
    #[case] read_only: bool,
    #[case] expected_recovered: usize,
    #[case] expected_state: JobState,
) {
    let dir = tempfile::tempdir().unwrap();
    let mut state = build_state(&Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    })
    .unwrap();
    Arc::get_mut(&mut state).unwrap().read_only = read_only;
    let id = state
        .meta
        .start_job_run(NewJobRun {
            kind: JobKind::CacheRefresh,
            scope: "pypi",
            repository: None,
            started_at_unix: 1,
        })
        .unwrap();

    assert_eq!(recover_job_attempts(&state).unwrap(), expected_recovered);
    assert_eq!(state.meta.get_job_run(&id).unwrap().unwrap().state, expected_state);
}

#[test]
fn test_build_state_reads_basic_upstream_credentials_from_netrc() {
    let dir = tempfile::tempdir().unwrap();
    let netrc = dir.path().join("credentials.netrc");
    write_netrc(
        &netrc,
        "machine https://corp.example:443 login reader password netrc-secret\n",
    );
    let state = build_state(&Config {
        data_dir: dir.path().join("data"),
        netrc: Some(netrc),
        indexes: vec![cached("corp", "https://corp.example/simple/")],
        ..Config::default()
    })
    .unwrap();
    let RuntimeKind::Cached { client, .. } = &state.indexes[0].kind else {
        panic!("expected cached index");
    };

    assert_eq!(
        current_auth(client),
        Auth::Basic {
            username: "reader".to_owned(),
            password: "netrc-secret".to_owned()
        }
    );
}

#[tokio::test]
async fn test_build_state_reads_netrc_for_routed_upstreams() {
    let dir = tempfile::tempdir().unwrap();
    let metadata = MockServer::start().await;
    let artifacts = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/packages/pkg.whl"))
        .and(match_header(
            "authorization",
            "Basic YXJ0aWZhY3QtcmVhZGVyOmFydGlmYWN0LXNlY3JldA==",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"wheelbytes".to_vec()))
        .mount(&artifacts)
        .await;
    let netrc = dir.path().join("credentials.netrc");
    write_netrc(
        &netrc,
        &format!(
            "machine {} login metadata-reader password metadata-secret\n\
             machine {} login artifact-reader password artifact-secret\n",
            metadata.uri(),
            artifacts.uri()
        ),
    );
    let state = build_state(&Config {
        data_dir: dir.path().join("data"),
        netrc: Some(netrc),
        indexes: vec![routed(
            &format!("{}/simple/", metadata.uri()),
            Some(&format!("{}/packages/", artifacts.uri())),
        )],
        ..Config::default()
    })
    .unwrap();
    let source = state.upstream_routes["pypi"].source("primary").unwrap();

    assert_eq!(
        current_auth(source.client()),
        Auth::Basic {
            username: "metadata-reader".to_owned(),
            password: "metadata-secret".to_owned()
        }
    );
    let chunks = source
        .artifacts()
        .stream_bytes(&format!("{}/pkg.whl", artifacts.uri()))
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(
        chunks
            .iter()
            .flat_map(|chunk| chunk.iter().copied())
            .collect::<Vec<_>>(),
        b"wheelbytes"
    );
}

#[tokio::test]
async fn test_routed_metadata_and_artifacts_share_credential_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let metadata = MockServer::start().await;
    let artifacts = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/pkg/"))
        .and(header_regex("authorization", "^Bearer old$"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&metadata)
        .await;
    Mock::given(method("GET"))
        .and(path("/simple/pkg/"))
        .and(header_regex("authorization", "^Bearer new$"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&metadata)
        .await;
    Mock::given(method("GET"))
        .and(path("/packages/pkg.whl"))
        .and(header_regex("authorization", "^Bearer new$"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"wheelbytes".to_vec()))
        .expect(1)
        .mount(&artifacts)
        .await;
    let token = dir.path().join("token");
    std::fs::write(&token, "old\n").unwrap();
    let mut index = routed(
        &format!("{}/simple/", metadata.uri()),
        Some(&format!("{}/packages/", artifacts.uri())),
    );
    let IndexKind::Cached { routing, .. } = &mut index.kind else {
        panic!("expected a routed cached index");
    };
    routing.upstreams[0].token = Some(SecretSource::File(token.clone()));
    routing.upstreams[0].credential_refresh = Some(CredentialRefreshConfig {
        interval: Duration::from_hours(1),
        on_unauthorized: true,
        failure: CredentialFailureMode::Fail,
    });
    let state = build_state(&Config {
        data_dir: dir.path().join("data"),
        indexes: vec![index],
        ..Config::default()
    })
    .unwrap();
    std::fs::write(token, "new\n").unwrap();
    let source = state.upstream_routes["pypi"].source("primary").unwrap();

    let metadata_response = source
        .client()
        .send_conditional(source.client().base().join("pkg/").unwrap(), "application/json", None)
        .await
        .unwrap();
    let artifact = source
        .artifacts()
        .stream_bytes(&format!("{}/pkg.whl", artifacts.uri()))
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap()
        .iter()
        .flat_map(|chunk| chunk.iter().copied())
        .collect::<Vec<_>>();

    assert_eq!(
        (metadata_response.status(), artifact.as_slice()),
        (StatusCode::OK, b"wheelbytes".as_slice())
    );
}

#[cfg(unix)]
#[tokio::test]
async fn test_exec_credential_authenticates_a_cached_upstream() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/pkg/"))
        .and(header_regex("authorization", "^Bearer exec-token$"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&upstream)
        .await;
    let (exec, executions, requests) = exec_credential_helper(&dir);
    let mut index = cached("corp", &format!("{}/simple/", upstream.uri()));
    let IndexKind::Cached { routing, .. } = &mut index.kind else {
        panic!("expected a cached index");
    };
    routing.upstreams[0].credential_exec = Some(exec);
    let state = build_state(&Config {
        data_dir: dir.path().join("data"),
        indexes: vec![index],
        ..Config::default()
    })
    .unwrap();
    let RuntimeKind::Cached { client, .. } = &state.indexes[0].kind else {
        panic!("expected a cached index");
    };

    let response = client
        .send_conditional(client.base().join("pkg/").unwrap(), "application/json", None)
        .await
        .unwrap();

    assert_eq!(
        (response.status(), std::fs::read(executions).unwrap().len()),
        (StatusCode::OK, 1)
    );
    assert_eq!(
        std::fs::read_to_string(requests).unwrap(),
        format!(r#"{{"version":1,"origin":"{}","scope":"read"}}"#, upstream.uri())
    );
}

#[cfg(unix)]
#[tokio::test]
async fn test_routed_metadata_and_artifacts_share_an_exec_credential() {
    let dir = tempfile::tempdir().unwrap();
    let metadata = MockServer::start().await;
    let artifacts = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/pkg/"))
        .and(header_regex("authorization", "^Bearer exec-token$"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&metadata)
        .await;
    Mock::given(method("GET"))
        .and(path("/packages/pkg.whl"))
        .and(header_regex("authorization", "^Bearer exec-token$"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"wheelbytes".to_vec()))
        .expect(1)
        .mount(&artifacts)
        .await;
    let (exec, executions, _) = exec_credential_helper(&dir);
    let mut index = routed(
        &format!("{}/simple/", metadata.uri()),
        Some(&format!("{}/packages/", artifacts.uri())),
    );
    let IndexKind::Cached { routing, .. } = &mut index.kind else {
        panic!("expected a routed cached index");
    };
    routing.upstreams[0].credential_exec = Some(exec);
    let state = build_state(&Config {
        data_dir: dir.path().join("data"),
        indexes: vec![index],
        ..Config::default()
    })
    .unwrap();
    let source = state.upstream_routes["pypi"].source("primary").unwrap();

    let metadata_response = source
        .client()
        .send_conditional(source.client().base().join("pkg/").unwrap(), "application/json", None)
        .await
        .unwrap();
    let artifact = source
        .artifacts()
        .stream_bytes(&format!("{}/pkg.whl", artifacts.uri()))
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap()
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

    assert_eq!(
        (
            metadata_response.status(),
            artifact,
            std::fs::read(executions).unwrap().len()
        ),
        (StatusCode::OK, b"wheelbytes".to_vec(), 1)
    );
}

#[tokio::test]
async fn test_refresh_failure_can_fall_back_to_anonymous() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/pkg/"))
        .and(header_regex("authorization", "^Bearer old$"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/simple/pkg/"))
        .and(NoAuthorization)
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&upstream)
        .await;
    let token = dir.path().join("token");
    std::fs::write(&token, "old\n").unwrap();
    let mut index = cached("corp", &format!("{}/simple/", upstream.uri()));
    let IndexKind::Cached { routing, .. } = &mut index.kind else {
        panic!("expected a cached index");
    };
    routing.upstreams[0].token = Some(SecretSource::File(token.clone()));
    routing.upstreams[0].credential_refresh = Some(CredentialRefreshConfig {
        interval: Duration::from_hours(1),
        on_unauthorized: true,
        failure: CredentialFailureMode::Anonymous,
    });
    let state = build_state(&Config {
        data_dir: dir.path().join("data"),
        indexes: vec![index],
        ..Config::default()
    })
    .unwrap();
    std::fs::remove_file(token).unwrap();
    let RuntimeKind::Cached { client, .. } = &state.indexes[0].kind else {
        panic!("expected a cached index");
    };

    let response = client
        .send_conditional(client.base().join("pkg/").unwrap(), "application/json", None)
        .await
        .unwrap();

    assert_eq!((response.status(), current_auth(client)), (StatusCode::OK, Auth::None));
}

#[rstest]
#[case::basic(
    Some("configured-reader"),
    Some(SecretSource::Literal("configured-secret".to_owned())),
    None,
    Auth::Basic { username: "configured-reader".to_owned(), password: "configured-secret".to_owned() }
)]
#[case::bearer(
    None,
    None,
    Some(SecretSource::Literal("configured-token".to_owned())),
    Auth::Bearer("configured-token".to_owned())
)]
fn test_build_state_prefers_explicit_upstream_credentials(
    #[case] username: Option<&str>,
    #[case] password: Option<SecretSource>,
    #[case] token: Option<SecretSource>,
    #[case] expected: Auth,
) {
    let dir = tempfile::tempdir().unwrap();
    let netrc = dir.path().join("credentials.netrc");
    write_netrc(
        &netrc,
        "machine https://corp.example:443 login netrc-reader password netrc-secret\n",
    );
    let mut index = cached("corp", "https://corp.example/simple/");
    let IndexKind::Cached { routing, .. } = &mut index.kind else {
        panic!("expected cached index");
    };
    routing.upstreams[0].username = username.map(str::to_owned);
    routing.upstreams[0].password = password;
    routing.upstreams[0].token = token;
    let state = build_state(&Config {
        data_dir: dir.path().join("data"),
        netrc: Some(netrc),
        indexes: vec![index],
        ..Config::default()
    })
    .unwrap();
    let RuntimeKind::Cached { client, .. } = &state.indexes[0].kind else {
        panic!("expected cached index");
    };

    assert_eq!(current_auth(client), expected);
}

#[test]
fn test_build_state_reads_an_upstream_token_from_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = cached("corp", "https://corp.example/simple/");
    let IndexKind::Cached { routing, .. } = &mut index.kind else {
        panic!("expected cached index");
    };
    routing.upstreams[0].token = Some(SecretSource::Env("PATH".to_owned()));
    let state = build_state(&Config {
        data_dir: dir.path().join("data"),
        indexes: vec![index],
        ..Config::default()
    })
    .unwrap();
    let RuntimeKind::Cached { client, .. } = &state.indexes[0].kind else {
        panic!("expected cached index");
    };

    assert_eq!(
        current_auth(client),
        Auth::Bearer(std::env::var("PATH").unwrap().trim().to_owned())
    );
}

#[test]
fn test_build_state_reports_a_missing_upstream_credential_environment_variable() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = cached("corp", "https://corp.example/simple/");
    let IndexKind::Cached { routing, .. } = &mut index.kind else {
        panic!("expected cached index");
    };
    routing.upstreams[0].token = Some(SecretSource::Env("PERYX_TEST_ABSENT_CREDENTIAL".to_owned()));
    let Err(error) = build_state(&Config {
        data_dir: dir.path().join("data"),
        indexes: vec![index],
        ..Config::default()
    }) else {
        panic!("expected a missing environment variable to fail startup");
    };
    let chain = format!("{error:#}");
    assert!(
        chain.contains(
            "credential environment variable PERYX_TEST_ABSENT_CREDENTIAL is unset, empty, or not valid UTF-8"
        ),
        "{chain}"
    );
}

#[test]
fn test_build_state_rejects_a_virtual_index_naming_an_unknown_layer() {
    let dir = tempfile::tempdir().unwrap();
    let Err(error) = build_state(&Config {
        data_dir: dir.path().join("data"),
        indexes: vec![hosted("store"), virtual_index(&["ghost"], None)],
        ..Config::default()
    }) else {
        panic!("expected an unresolved virtual layer to fail startup");
    };

    assert!(error.to_string().contains("unknown index ghost"), "{error}");
}

#[test]
fn test_build_state_leaves_missing_netrc_entries_anonymous() {
    let dir = tempfile::tempdir().unwrap();
    let netrc = dir.path().join("credentials.netrc");
    write_netrc(&netrc, "machine other.example login reader password secret\n");
    let state = build_state(&Config {
        data_dir: dir.path().join("data"),
        netrc: Some(netrc),
        indexes: vec![cached("corp", "https://corp.example/simple/")],
        ..Config::default()
    })
    .unwrap();
    let RuntimeKind::Cached { client, .. } = &state.indexes[0].kind else {
        panic!("expected cached index");
    };

    assert_eq!(current_auth(client), Auth::None);
}

#[test]
fn test_build_state_reports_netrc_errors_without_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let netrc = dir.path().join("credentials.netrc");
    write_netrc(
        &netrc,
        "machine corp.example login reader password swordfish invalid-token\n",
    );
    let Err(error) = build_state(&Config {
        data_dir: dir.path().join("data"),
        netrc: Some(netrc),
        indexes: vec![cached("corp", "https://corp.example/simple/")],
        ..Config::default()
    }) else {
        panic!("expected invalid netrc syntax to fail startup");
    };
    let message = format!("{error:#}");

    assert!(message.contains("load upstream netrc"));
    assert!(message.contains("has invalid syntax"));
    assert!(!message.contains("swordfish"));
}

#[test]
fn test_build_state_reports_an_unreadable_netrc_path() {
    let dir = tempfile::tempdir().unwrap();
    let netrc = dir.path().join("missing.netrc");
    let Err(error) = build_state(&Config {
        data_dir: dir.path().join("data"),
        netrc: Some(netrc.clone()),
        indexes: vec![cached("corp", "https://corp.example/simple/")],
        ..Config::default()
    }) else {
        panic!("expected a missing netrc file to fail startup");
    };
    let message = format!("{error:#}");

    assert!(message.contains("load upstream netrc"));
    assert!(message.contains(&netrc.display().to_string()));
    assert!(message.contains("cannot read netrc file"));
}

#[cfg(unix)]
#[test]
fn test_build_state_rejects_an_insecure_netrc_mode() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let netrc = dir.path().join("public.netrc");
    write_netrc(&netrc, "machine corp.example login reader password swordfish\n");
    std::fs::set_permissions(&netrc, std::fs::Permissions::from_mode(0o640)).unwrap();
    let Err(error) = build_state(&Config {
        data_dir: dir.path().join("data"),
        netrc: Some(netrc),
        indexes: vec![cached("corp", "https://corp.example/simple/")],
        ..Config::default()
    }) else {
        panic!("expected an insecure netrc mode to fail startup");
    };
    let message = format!("{error:#}");

    assert!(message.contains("must not grant group or other permissions"));
    assert!(!message.contains("swordfish"));
}

#[cfg(unix)]
#[test]
fn test_build_state_rejects_a_netrc_owned_by_another_user() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let dir = tempfile::tempdir().unwrap();
    let foreign = dir.path().join("foreign.netrc");
    write_netrc(&foreign, "machine corp.example login reader password swordfish\n");
    std::fs::set_permissions(&foreign, std::fs::Permissions::from_mode(0o644)).unwrap();
    let path = if std::os::unix::fs::chown(&foreign, Some(std::fs::metadata(&foreign).unwrap().uid() ^ 1), None).is_ok()
    {
        foreign
    } else {
        PathBuf::from("/etc/hosts")
    };
    let Err(error) = build_state(&Config {
        data_dir: dir.path().join("data"),
        netrc: Some(path),
        indexes: vec![cached("corp", "https://corp.example/simple/")],
        ..Config::default()
    }) else {
        panic!("expected a netrc owned by another user to fail startup");
    };

    assert!(format!("{error:#}").contains("must be owned by the effective user"));
}

#[test]
fn test_build_state_claims_configured_writer_identity() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some("writer-a".to_owned()),
        ..Config::default()
    };

    let state = build_state(&config).unwrap();

    assert_eq!(state.meta.writer_identity().unwrap().as_deref(), Some("writer-a"));
}

#[test]
fn test_build_state_records_policy_decisions() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = hosted("private");
    index.policy.block_projects = vec!["blocked".to_owned()];
    let state = build_state(&config_with(&dir, vec![index])).unwrap();

    state.indexes[0]
        .policy
        .check_facts(
            peryx_policy::PolicyAction::Serve,
            &peryx_policy::ArtifactFacts {
                project: "blocked".to_owned(),
                filename: Some("blocked-1.0.whl".to_owned()),
                version: Some("1.0".to_owned()),
                source: Some("pypi".to_owned()),
                ..peryx_policy::ArtifactFacts::default()
            },
        )
        .unwrap_err();
    let mut record = serde_json::to_value(
        &state
            .meta
            .query_policy_decisions(&PolicyDecisionQuery {
                repository: Some("private".to_owned()),
                limit: 1,
                ..PolicyDecisionQuery::default()
            })
            .unwrap()
            .decisions[0]
            .record,
    )
    .unwrap();
    let object = record.as_object_mut().unwrap();
    object.remove("id");
    object.remove("evaluated_at_unix");

    assert_eq!(
        record,
        serde_json::json!({
            "repository": "private",
            "project": "blocked",
            "version": "1.0",
            "filename": "blocked-1.0.whl",
            "source": "pypi",
            "action": "serve",
            "state": "deny",
            "rule": "project-block-list",
            "reason": "project \"blocked\" is blocked",
            "input_generation": {"repository": 0, "catalog": 0, "policy": 1},
            "next_eligible_at_unix": null
        })
    );
}

#[test]
fn test_policy_recording_failure_does_not_change_the_decision() {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&config_with(&dir, vec![hosted("private")])).unwrap();

    assert_eq!(
        (
            state.indexes[0]
                .policy
                .check_project(peryx_policy::PolicyAction::Serve, &"x".repeat(513)),
            state
                .meta
                .query_policy_decisions(&PolicyDecisionQuery {
                    repository: Some("private".to_owned()),
                    limit: 1,
                    ..PolicyDecisionQuery::default()
                })
                .unwrap()
                .decisions,
        ),
        (Ok(()), Vec::new())
    );
}

#[test]
fn test_build_state_rejects_a_competing_writer_identity() {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    store.claim_writer_identity("writer-a").unwrap();
    drop(store);
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some("writer-b".to_owned()),
        ..Config::default()
    };

    let Err(error) = build_state(&config) else {
        panic!("expected writer identity conflict");
    };

    let message = format!("{error:#}");
    assert!(message.contains("claim writer identity \"writer-b\""), "{message}");
    assert!(message.contains("claimed by writer \"writer-a\""), "{message}");
}

#[test]
fn test_build_state_makes_replica_upstreams_offline() {
    let dir = tempfile::tempdir().unwrap();
    claim_writer(&dir, "writer-a");
    let state = build_state(&Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some("writer-a".to_owned()),
        read_only: true,
        ..Config::default()
    })
    .unwrap();
    assert!(state.read_only);
    assert!(state.indexes.iter().all(|index| match &index.kind {
        peryx_driver::IndexKind::Cached { offline, .. } => *offline,
        peryx_driver::IndexKind::Hosted { .. } | peryx_driver::IndexKind::Virtual { .. } => true,
    }));
}

#[rstest]
#[case::read_only(false)]
#[case::replication(true)]
fn test_build_state_rejects_a_replica_without_writer_identity(#[case] configured_replication: bool) {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        read_only: !configured_replication,
        availability: availability_replica(configured_replication),
        ..Config::default()
    };

    let Err(error) = build_state(&config) else {
        panic!("expected invalid replica configuration");
    };

    assert_eq!(
        format!("{error:#}"),
        "validate configuration: writer identity: required in read replica mode"
    );
    assert!(!dir.path().join("peryx.redb").exists());
}

#[rstest]
#[case::read_only(false)]
#[case::replication(true)]
fn test_build_state_replica_does_not_claim_writer_identity(#[case] configured_replication: bool) {
    let dir = tempfile::tempdir().unwrap();
    claim_writer(&dir, "writer-a");

    let state = build_state(&Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some("writer-a".to_owned()),
        read_only: !configured_replication,
        availability: availability_replica(configured_replication),
        ..Config::default()
    })
    .unwrap();

    assert!(state.read_only);
    assert_eq!(state.meta.writer_identity().unwrap().as_deref(), Some("writer-a"));
}

#[rstest]
#[case::missing(None, "None")]
#[case::different(Some("writer-b"), "Some(\"writer-b\")")]
fn test_build_state_rejects_a_replica_with_an_unmatched_writer(
    #[case] active: Option<&str>,
    #[case] expected: &str,
    #[values(false, true)] configured_replication: bool,
) {
    let dir = tempfile::tempdir().unwrap();
    if let Some(active) = active {
        claim_writer(&dir, active);
    }
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some("writer-a".to_owned()),
        read_only: !configured_replication,
        availability: availability_replica(configured_replication),
        ..Config::default()
    };

    let Err(error) = build_state(&config) else {
        panic!("expected replica writer identity conflict");
    };

    assert_eq!(
        error.to_string(),
        format!("configured replica writer Some(\"writer-a\") does not match metadata store writer {expected}")
    );
}

#[test]
fn test_build_state_applies_upstream_concurrency() {
    let dir = tempfile::tempdir().unwrap();
    let mut pypi = cached("pypi", "https://pypi.org/simple/");
    let IndexKind::Cached {
        upstream_concurrency, ..
    } = &mut pypi.kind
    else {
        panic!("expected cached index");
    };
    *upstream_concurrency = 2;
    let config = config_with(&dir, vec![pypi]);

    let state = build_state(&config).unwrap();

    let snapshots = state.upstream_limits.snapshots();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].max_concurrent, 2);
}

#[test]
fn test_build_state_reports_metadata_store_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("peryx.redb")).unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };

    let Err(err) = build_state(&config) else {
        panic!("expected metadata store error");
    };

    assert!(err.to_string().contains("open metadata store"));
}

#[test]
fn test_build_state_reports_index_errors() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with(&dir, vec![cached("pypi", "not a url")]);

    let Err(err) = build_state(&config) else {
        panic!("expected index error");
    };

    assert!(err.to_string().contains("build cached index pypi"));
}

#[test]
fn test_build_state_reports_webhook_errors() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = hosted("hosted");
    index.webhooks.push(WebhookConfig {
        name: "ci".to_owned(),
        url: "ftp://ci.example/hook".to_owned(),
        secret: WebhookSecret::Literal("secret".to_owned()),
        events: Vec::new(),
    });
    let config = config_with(&dir, vec![index]);

    let Err(err) = build_state(&config) else {
        panic!("expected webhook error");
    };

    assert!(err.to_string().contains("build webhook targets"));
}

#[test]
fn test_build_state_reports_missing_webhook_secret_env() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = hosted("hosted");
    index.webhooks.push(WebhookConfig {
        name: "ci".to_owned(),
        url: "https://ci.example/hook".to_owned(),
        secret: WebhookSecret::Env("PERYX_TEST_MISSING_WEBHOOK_SECRET".to_owned()),
        events: Vec::new(),
    });
    let config = config_with(&dir, vec![index]);

    let Err(err) = build_state(&config) else {
        panic!("expected webhook env error");
    };

    assert!(
        err.to_string()
            .contains("read webhook secret env var PERYX_TEST_MISSING_WEBHOOK_SECRET")
    );
}

#[test]
fn test_check_config_accepts_the_default_topology() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with(&dir, vec![hosted("hosted")]);

    check_config(&config).unwrap();
}

#[rstest]
#[case::cross_field(blank_writer_config, "validate configuration")]
#[case::logging(file_log_config, "validate logging configuration")]
#[case::index_topology(bad_upstream_config, "build cached index pypi")]
#[case::ecosystem_settings(bad_settings_config, "unknown field `bogus`")]
#[case::webhook(bad_webhook_config, "build webhook targets")]
fn test_check_config_reports_the_error_serve_would_hit(
    #[case] build: fn(&tempfile::TempDir) -> Config,
    #[case] needle: &str,
) {
    let dir = tempfile::tempdir().unwrap();

    let err = check_config(&build(&dir)).unwrap_err();

    assert!(err.to_string().contains(needle), "{err}");
}

fn blank_writer_config(dir: &tempfile::TempDir) -> Config {
    Config {
        writer_identity: Some("   ".to_owned()),
        ..config_with(dir, vec![hosted("hosted")])
    }
}

fn file_log_config(dir: &tempfile::TempDir) -> Config {
    let mut config = config_with(dir, vec![hosted("hosted")]);
    config.log.sink = LogSink::File;
    config
}

fn bad_upstream_config(dir: &tempfile::TempDir) -> Config {
    config_with(dir, vec![cached("pypi", "not a url")])
}

fn bad_settings_config(dir: &tempfile::TempDir) -> Config {
    let mut index = hosted("images");
    index.ecosystem = peryx_ecosystem_oci::ECOSYSTEM;
    index
        .ecosystem_settings
        .insert("bogus".to_owned(), toml::Value::from("x"));
    config_with(dir, vec![index])
}

fn bad_webhook_config(dir: &tempfile::TempDir) -> Config {
    let mut index = hosted("hosted");
    index.webhooks.push(WebhookConfig {
        name: "ci".to_owned(),
        url: "ftp://ci.example/hook".to_owned(),
        secret: WebhookSecret::Literal("secret".to_owned()),
        events: Vec::new(),
    });
    config_with(dir, vec![index])
}

#[test]
fn test_build_state_wires_the_token_realm_signing_key() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        auth: AuthConfig {
            signing_key: Some(SecretSource::Literal("super-secret".to_owned())),
            token_ttl_secs: 900,
            ..AuthConfig::default()
        },
        ..Config::default()
    };

    let state = build_state(&config).unwrap();

    assert!(state.signer.is_some());
    assert_eq!(state.token_ttl_secs, 900);
}

fn ldap_provider(bind: LdapBindConfig) -> LdapProviderConfig {
    LdapProviderConfig {
        id: ProviderId::new("staff").unwrap(),
        url: url::Url::parse("ldap://127.0.0.1:9").unwrap(),
        base_dn: "ou=people,dc=example,dc=com".to_owned(),
        bind,
        subject_attribute: "entryUUID".to_owned(),
        display_name_attribute: "displayName".to_owned(),
        group_attribute: None,
        ca_file: None,
        connect_timeout: Duration::from_secs(1),
        request_timeout: Duration::from_secs(1),
        max_connections: NonZeroU32::new(2).unwrap(),
        group_mappings: Vec::new(),
    }
}

fn oidc_provider(id: &str, client_secret: Option<SecretSource>) -> OidcProviderConfig {
    OidcProviderConfig {
        id: ProviderId::new(id).unwrap(),
        issuer: url::Url::parse("https://idp.example/realms/main").unwrap(),
        client_id: "peryx".to_owned(),
        client_secret,
        redirect_uri: url::Url::parse("https://registry.example/oidc/callback").unwrap(),
        scopes: vec!["openid".to_owned()],
        subject_claim: "sub".to_owned(),
        display_name_claim: "name".to_owned(),
        groups_claim: None,
        clock_skew: Duration::from_mins(1),
        request_timeout: Duration::from_secs(5),
        group_mappings: Vec::new(),
    }
}

#[test]
fn test_build_state_installs_lazy_named_oidc_logins() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        auth: AuthConfig {
            oidc_providers: vec![
                oidc_provider("corporate", Some(SecretSource::Literal("client-secret".to_owned()))),
                oidc_provider("partners", None),
            ],
            signing_key: Some(SecretSource::Literal("realm-key".to_owned())),
            ..AuthConfig::default()
        },
        ..Config::default()
    };

    let state = build_state(&config).unwrap();

    assert_eq!(state.oidc_login("corporate").unwrap().id().as_str(), "corporate");
    assert_eq!(state.oidc_login("partners").unwrap().id().as_str(), "partners");
    assert!(state.oidc_login("missing").is_none());
    assert_eq!(state.oidc_providers(), vec!["corporate", "partners"]);
}

#[test]
fn test_build_state_reports_an_unreadable_oidc_client_secret() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        auth: AuthConfig {
            oidc_providers: vec![oidc_provider(
                "corporate",
                Some(SecretSource::File(PathBuf::from("/nonexistent/peryx/oidc-secret"))),
            )],
            signing_key: Some(SecretSource::Literal("realm-key".to_owned())),
            ..AuthConfig::default()
        },
        ..Config::default()
    };

    let Err(error) = build_state(&config) else {
        panic!("expected OIDC client secret error");
    };

    assert!(error.to_string().contains("read OIDC provider corporate client secret"));
}

#[test]
fn test_build_state_rejects_an_invalid_oidc_provider() {
    let dir = tempfile::tempdir().unwrap();
    let mut provider = oidc_provider("corporate", None);
    provider.issuer = url::Url::parse("https://idp.example/?tenant=main").unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        auth: AuthConfig {
            oidc_providers: vec![provider],
            signing_key: Some(SecretSource::Literal("realm-key".to_owned())),
            ..AuthConfig::default()
        },
        ..Config::default()
    };

    let Err(error) = build_state(&config) else {
        panic!("expected invalid OIDC provider error");
    };

    assert_eq!(error.to_string(), "configure OIDC provider corporate");
}

#[test]
fn test_build_state_installs_lazy_named_ldap_logins() {
    let dir = tempfile::tempdir().unwrap();
    let mut search = ldap_provider(LdapBindConfig::Search {
        username_attribute: "uid".to_owned(),
        bind_dn: "cn=service,dc=example,dc=com".to_owned(),
        bind_password: SecretSource::Literal("directory-secret".to_owned()),
    });
    search.id = ProviderId::new("contractors").unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        auth: AuthConfig {
            ldap_providers: vec![
                ldap_provider(LdapBindConfig::Direct {
                    dn_attribute: "uid".to_owned(),
                }),
                search,
            ],
            ..AuthConfig::default()
        },
        ..Config::default()
    };

    let state = build_state(&config).unwrap();

    assert_eq!(state.ldap_login("staff").unwrap().id().as_str(), "staff");
    assert_eq!(state.ldap_login("contractors").unwrap().id().as_str(), "contractors");
    assert!(state.ldap_login("missing").is_none());
}

#[test]
fn test_build_state_reports_an_unreadable_ldap_bind_password() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        auth: AuthConfig {
            ldap_providers: vec![ldap_provider(LdapBindConfig::Search {
                username_attribute: "uid".to_owned(),
                bind_dn: "cn=service,dc=example,dc=com".to_owned(),
                bind_password: SecretSource::File(PathBuf::from("/nonexistent/peryx/ldap-password")),
            })],
            ..AuthConfig::default()
        },
        ..Config::default()
    };

    let Err(error) = build_state(&config) else {
        panic!("expected LDAP bind password error");
    };

    assert!(error.to_string().contains("read LDAP provider staff bind password"));
}

#[test]
fn test_build_state_rejects_an_invalid_ldap_ca() {
    let dir = tempfile::tempdir().unwrap();
    let ca = dir.path().join("ldap-ca.pem");
    std::fs::write(&ca, "not a certificate").unwrap();
    let mut provider = ldap_provider(LdapBindConfig::Direct {
        dn_attribute: "uid".to_owned(),
    });
    provider.ca_file = Some(ca);
    let config = Config {
        data_dir: dir.path().join("data"),
        auth: AuthConfig {
            ldap_providers: vec![provider],
            ..AuthConfig::default()
        },
        ..Config::default()
    };

    let Err(error) = build_state(&config) else {
        panic!("expected invalid LDAP CA error");
    };

    assert_eq!(error.to_string(), "configure LDAP provider staff");
}

#[test]
fn test_build_state_bounds_ldap_ca_reads() {
    let dir = tempfile::tempdir().unwrap();
    let ca = dir.path().join("ldap-ca.pem");
    std::fs::write(&ca, vec![b'x'; (1 << 20) + 1]).unwrap();
    let mut provider = ldap_provider(LdapBindConfig::Direct {
        dn_attribute: "uid".to_owned(),
    });
    provider.ca_file = Some(ca);
    let config = Config {
        data_dir: dir.path().join("data"),
        auth: AuthConfig {
            ldap_providers: vec![provider],
            ..AuthConfig::default()
        },
        ..Config::default()
    };

    let Err(error) = build_state(&config) else {
        panic!("expected oversized LDAP CA error");
    };

    assert_eq!(error.to_string(), "read LDAP provider staff CA");
}

#[test]
fn test_build_state_reports_an_unreadable_ldap_ca() {
    let dir = tempfile::tempdir().unwrap();
    let mut provider = ldap_provider(LdapBindConfig::Direct {
        dn_attribute: "uid".to_owned(),
    });
    provider.ca_file = Some(PathBuf::from("/nonexistent/peryx/ldap-ca.pem"));
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        auth: AuthConfig {
            ldap_providers: vec![provider],
            ..AuthConfig::default()
        },
        ..Config::default()
    };

    let Err(error) = build_state(&config) else {
        panic!("expected LDAP CA read error");
    };

    assert_eq!(error.to_string(), "read LDAP provider staff CA");
}

#[test]
fn test_build_state_installs_trusted_publishing_for_a_resolved_route() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        indexes: vec![hosted("private")],
        auth: AuthConfig {
            signing_key: Some(SecretSource::Literal("super-secret".to_owned())),
            oidc_audience: "packages.example".to_owned(),
            trusted_publishers: vec![TrustedPublisherConfig {
                id: "release".to_owned(),
                issuer: "https://issuer.example".to_owned(),
                repository: "private".to_owned(),
                subject: "repo:org/app:*".to_owned(),
                projects: vec!["app".to_owned()],
                claims: std::collections::BTreeMap::new(),
            }],
            ..AuthConfig::default()
        },
        ..Config::default()
    };

    let state = build_state(&config).unwrap();

    assert_eq!(
        state.trusted_publishing.as_ref().unwrap().audience(),
        "packages.example"
    );
}

#[test]
fn test_build_state_rejects_a_trusted_publisher_with_an_unknown_repository() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        indexes: vec![hosted("private")],
        auth: AuthConfig {
            signing_key: Some(SecretSource::Literal("super-secret".to_owned())),
            oidc_audience: "packages.example".to_owned(),
            trusted_publishers: vec![TrustedPublisherConfig {
                id: "release".to_owned(),
                issuer: "https://issuer.example".to_owned(),
                repository: "missing".to_owned(),
                subject: "repo:org/app:*".to_owned(),
                projects: vec!["app".to_owned()],
                claims: std::collections::BTreeMap::new(),
            }],
            ..AuthConfig::default()
        },
        ..Config::default()
    };

    let Err(error) = build_state(&config) else {
        panic!("expected an unknown-repository rejection");
    };

    assert_eq!(
        error.chain().map(ToString::to_string).collect::<Vec<_>>().join(": "),
        "validate configuration: trusted publisher release: repository must name a writable index with trusted publishing support"
    );
}

#[test]
fn test_build_state_reports_an_unreadable_signing_key_file() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        auth: AuthConfig {
            signing_key: Some(SecretSource::File(PathBuf::from("/nonexistent/peryx/signing-key"))),
            ..AuthConfig::default()
        },
        ..Config::default()
    };

    let Err(err) = build_state(&config) else {
        panic!("expected signing-key read error");
    };

    assert!(err.to_string().contains("read the token realm signing key"), "{err}");
}

#[rstest]
#[case::literal(empty_literal_signing_key, "token realm signing key must not be empty")]
#[case::file(empty_file_signing_key, "read the token realm signing key")]
fn test_build_state_rejects_an_empty_signing_key(#[case] source: fn(&Path) -> SecretSource, #[case] expected: &str) {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().join("data"),
        auth: AuthConfig {
            signing_key: Some(source(dir.path())),
            ..AuthConfig::default()
        },
        ..Config::default()
    };

    let Err(err) = build_state(&config) else {
        panic!("expected empty signing-key error");
    };

    assert_eq!(err.to_string(), expected);
}

fn empty_literal_signing_key(_: &Path) -> SecretSource {
    SecretSource::Literal(" \n".to_owned())
}

fn empty_file_signing_key(dir: &Path) -> SecretSource {
    let path = dir.join("signing-key");
    std::fs::write(&path, " \n").unwrap();
    SecretSource::File(path)
}

#[tokio::test]
async fn test_build_state_starts_webhook_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = hosted("hosted");
    index.webhooks.push(WebhookConfig {
        name: "ci".to_owned(),
        url: "https://ci.example/hook".to_owned(),
        secret: WebhookSecret::Literal("secret".to_owned()),
        events: Vec::new(),
    });
    let config = config_with(&dir, vec![index]);

    let state = build_state(&config).unwrap();

    assert!(!state.webhooks.is_empty());
}

#[rstest]
#[case::bearer_takes_precedence(Some("tok"), Some("u"), Some("p"), Auth::Bearer("tok".to_owned()))]
#[case::basic(None, Some("u"), Some("p"), Auth::Basic { username: "u".to_owned(), password: "p".to_owned() })]
#[case::none(None, None, None, Auth::None)]
fn test_upstream_auth(
    #[case] token: Option<&str>,
    #[case] user: Option<&str>,
    #[case] pass: Option<&str>,
    #[case] expected: Auth,
) {
    assert_eq!(upstream_auth(token, user, pass), expected);
}

#[test]
fn test_build_router_data_dir_error() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("blocker");
    std::fs::write(&file, "x").unwrap();
    let config = Config {
        data_dir: file.join("sub"),
        ..Config::default()
    };
    let err = build_router(&config).unwrap_err();
    assert!(err.to_string().contains("create data directory"));
}

#[rstest]
#[case::metadata("not a url", None)]
#[case::artifact("https://metadata.example/simple/", Some("not a url"))]
fn test_build_state_rejects_invalid_routed_source_urls(#[case] metadata: &str, #[case] artifact: Option<&str>) {
    let dir = tempfile::tempdir().unwrap();
    let netrc = dir.path().join("credentials.netrc");
    write_netrc(&netrc, "default login reader password swordfish\n");

    let Err(err) = build_state(&Config {
        netrc: Some(netrc),
        ..config_with(&dir, vec![routed(metadata, artifact)])
    }) else {
        panic!("invalid routed source URL succeeded");
    };

    let message = format!("{err:#}");
    assert!(message.contains("match netrc credentials for <invalid upstream URL>"));
    assert!(!message.contains("swordfish"));
}

#[test]
fn test_build_state_rejects_invalid_routed_metadata_without_netrc() {
    let dir = tempfile::tempdir().unwrap();
    let Err(error) = build_state(&config_with(&dir, vec![routed("not a url", None)])) else {
        panic!("invalid routed source URL succeeded");
    };

    let message = format!("{error:#}");
    assert!(message.contains("build cached index pypi with upstream <invalid upstream URL>"));
}

#[rstest]
#[case::bad_upstream(
    || vec![cached("pypi", "not a url")],
    &["build cached index pypi", "<invalid upstream URL>"][..]
)]
#[case::invalid_policy(
    || {
        let mut index = cached("pypi", "https://pypi.org/simple/");
        index
            .ecosystem_policy
            .insert("allow_versions".to_owned(), "not a specifier".into());
        vec![index]
    },
    &["compile policy for pypi"][..]
)]
#[case::unknown_policy_key(
    || {
        let mut index = cached("pypi", "https://pypi.org/simple/");
        index.ecosystem_policy.insert("bogus".to_owned(), 1.into());
        vec![index]
    },
    &["compile policy for pypi", "unknown field `bogus`"][..]
)]
#[case::unsupported_policy(
    || {
        let mut index = cached("oci", "https://registry.example/v2/");
        index.ecosystem = peryx_ecosystem_oci::ECOSYSTEM;
        index.ecosystem_policy.insert("rule".to_owned(), true.into());
        vec![index]
    },
    &["the oci ecosystem does not support artifact policy"][..]
)]
#[case::duplicate_name(|| vec![hosted("a"), hosted("a")], &["duplicate index name"][..])]
#[case::duplicate_route(
    || {
        let mut second = hosted("b");
        second.route = "a".to_owned();
        vec![hosted("a"), second]
    },
    &["duplicate index route"][..]
)]
#[case::unsafe_route(
    || {
        let mut index = hosted("safe");
        index.route = "root/../pypi".to_owned();
        vec![index]
    },
    &["invalid index route root/../pypi"][..]
)]
#[case::reserved_route(
    || {
        let mut index = hosted("safe");
        index.route = "browse/private".to_owned();
        vec![index]
    },
    &["invalid index route browse/private"][..]
)]
#[case::unknown_layer(
    || vec![hosted("x"), virtual_index(&["ghost"], None)],
    &["unknown index ghost"][..]
)]
#[case::non_local_upload_target(
    || vec![cached("pypi", "https://pypi.org/simple/"), virtual_index(&["pypi"], Some("pypi"))],
    &["not a hosted index"][..]
)]
fn test_build_indexes_rejects(#[case] indexes: fn() -> Vec<IndexConfig>, #[case] expected: &[&str]) {
    let err = build_indexes(&indexes(), &AuthConfig::default(), false).unwrap_err();
    let message = err.to_string();
    for substr in expected {
        assert!(message.contains(substr), "{message}");
    }
}

#[test]
fn test_build_index_settings_surfaces_plugin_errors() {
    let mut index = cached("cache", "https://packages.example/");
    index.ecosystem_settings.insert("unknown".to_owned(), true.into());
    let message = build_index_settings(&[index]).unwrap_err().to_string();
    assert!(message.contains("compile settings for cache"), "{message}");
    assert!(message.contains("unknown field `unknown`"), "{message}");
}

#[test]
fn test_build_indexes_reports_an_unreadable_secret_file() {
    let mut index = hosted("store");
    index.tokens = vec![crate::tests::writer_token(SecretSource::File(PathBuf::from(
        "/nonexistent/peryx/token",
    )))];

    let err = build_indexes(&[index], &AuthConfig::default(), false).unwrap_err();

    assert!(
        err.to_string().contains("read the access rules of index store"),
        "{err}"
    );
}

#[test]
fn test_build_indexes_reads_upstream_credentials_from_files() {
    let dir = tempfile::tempdir().unwrap();
    let password = dir.path().join("password");
    let token = dir.path().join("token");
    std::fs::write(&password, "mirror-secret\n").unwrap();
    std::fs::write(&token, "upstream-token\n").unwrap();
    let mut index = cached("corp", "https://corp/simple/");
    let IndexKind::Cached { routing, .. } = &mut index.kind else {
        panic!("expected a cached index");
    };
    routing.upstreams[0].password = Some(SecretSource::File(password));
    routing.upstreams[0].token = Some(SecretSource::File(token));

    let indexes = build_indexes(&[index], &AuthConfig::default(), false).unwrap();

    assert!(matches!(&indexes[0].kind, RuntimeKind::Cached { .. }));
}

#[test]
fn test_build_state_installs_normalized_upstream_routes() {
    let dir = tempfile::tempdir().unwrap();
    let partial = crate::config::from_toml(
        PathBuf::from("x.toml"),
        r#"
[[index]]
name = "pypi"
protected = ["Internal.Pkg"]

[index.pins]
flask = "public"

[[index.upstream]]
name = "internal"
url = "https://packages.example/simple/"

[[index.upstream]]
name = "public"
url = "https://pypi.org/simple/"
"#,
    )
    .unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default().apply(partial).unwrap()
    };

    let state = build_state(&config).unwrap();
    let router = &state.upstream_routes["pypi"];

    assert_eq!(
        router
            .candidates("internal-pkg")
            .map(peryx_upstream::NamedUpstream::name)
            .collect::<Vec<_>>(),
        ["internal"]
    );
    assert_eq!(
        router
            .candidates("flask")
            .map(peryx_upstream::NamedUpstream::name)
            .collect::<Vec<_>>(),
        ["public"]
    );
}

#[test]
fn test_build_indexes_reports_unreadable_upstream_credentials() {
    let mut index = cached("corp", "https://corp/simple/");
    let IndexKind::Cached { routing, .. } = &mut index.kind else {
        panic!("expected a cached index");
    };
    routing.upstreams[0].password = Some(SecretSource::File(PathBuf::from("/nonexistent/peryx/password")));

    let err = build_indexes(&[index], &AuthConfig::default(), false).unwrap_err();

    assert!(
        err.to_string().contains("read the upstream credentials of index corp"),
        "{err}"
    );
}

#[test]
fn test_build_indexes_defaults_upload_to_first_local_layer() {
    let configs = [
        cached("pypi", "https://pypi.org/simple/"),
        hosted("store"),
        virtual_index(&["pypi", "store"], None),
    ];
    let indexes = build_indexes(&configs, &AuthConfig::default(), false).unwrap();
    let RuntimeKind::Virtual { upload, layers } = &indexes[2].kind else {
        panic!("expected virtual index");
    };
    assert_eq!(*upload, Some(1)); // "store" is the first hosted layer
    assert_eq!(layers, &[0, 1]);
}

#[test]
fn test_build_indexes_overlay_without_local_layer_has_no_upload() {
    let configs = [
        cached("pypi", "https://pypi.org/simple/"),
        virtual_index(&["pypi"], None),
    ];
    let indexes = build_indexes(&configs, &AuthConfig::default(), false).unwrap();
    let RuntimeKind::Virtual { upload, .. } = &indexes[1].kind else {
        panic!("expected virtual index");
    };
    assert_eq!(*upload, None);
}

fn dc_member(node: &str, dc: &str, address: &str, role: DcRole) -> DcMember {
    DcMember {
        node: node.to_owned(),
        dc: dc.to_owned(),
        address: address.to_owned(),
        role,
    }
}

fn dc_config(members: Vec<DcMember>, identity: &str, token: SecretSource) -> Config {
    Config {
        writer_identity: Some(identity.to_owned()),
        availability: AvailabilityConfig::Dc(ReplicationConfig::Primary {
            source: "ingress".to_owned(),
            token,
        }),
        dc_membership: Some(DcMembership {
            group: "group".to_owned(),
            members,
        }),
        ..Config::default()
    }
}

#[test]
fn test_receipt_sources_are_empty_without_a_roster() {
    assert!(receipt_sources(&Config::default()).unwrap().is_empty());
}

#[test]
fn test_receipt_sources_are_empty_when_the_node_is_absent_from_the_roster() {
    let config = dc_config(
        vec![dc_member("peer", "east", "http://peer/", DcRole::Replica)],
        "ghost",
        SecretSource::Literal("t".to_owned()),
    );

    assert!(receipt_sources(&config).unwrap().is_empty());
}

#[test]
fn test_receipt_sources_cover_same_dc_peers_excluding_self_and_other_datacenters() {
    let config = dc_config(
        vec![
            dc_member("local", "east", "http://local/", DcRole::Writer),
            dc_member("peer-1", "east", "http://peer1/", DcRole::Replica),
            dc_member("peer-2", "east", "http://peer2/", DcRole::Replica),
            dc_member("far", "west", "http://far/", DcRole::Replica),
        ],
        "local",
        SecretSource::Literal("t".to_owned()),
    );

    let nodes: std::collections::BTreeSet<String> = receipt_sources(&config)
        .unwrap()
        .iter()
        .map(|source| source.node().to_owned())
        .collect();

    assert_eq!(
        nodes,
        std::collections::BTreeSet::from(["peer-1".to_owned(), "peer-2".to_owned()]),
        "the local node and a west peer never appear in the same-DC gather",
    );
}

#[test]
fn test_receipt_sources_resolve_the_local_datacenter_from_the_node_identity() {
    // writer_identity is the shared writer every node claims, so a west replica must find its same-DC
    // peers through node_identity. writer_identity names the east writer here; resolving the gather from
    // it would wrongly return east peers, so the west member set proves node_identity wins.
    let config = Config {
        node_identity: Some("west-1".to_owned()),
        ..dc_config(
            vec![
                dc_member("east-writer", "east", "http://east/", DcRole::Writer),
                dc_member("west-1", "west", "http://west1/", DcRole::Replica),
                dc_member("west-2", "west", "http://west2/", DcRole::Replica),
            ],
            "east-writer",
            SecretSource::Literal("t".to_owned()),
        )
    };

    let nodes: std::collections::BTreeSet<String> = receipt_sources(&config)
        .unwrap()
        .iter()
        .map(|source| source.node().to_owned())
        .collect();

    assert_eq!(
        nodes,
        std::collections::BTreeSet::from(["west-2".to_owned()]),
        "the node_identity datacenter (west) sets the same-DC gather, not the writer's (east)",
    );
}

#[test]
fn test_receipt_sources_surface_an_unreadable_token() {
    let config = dc_config(
        vec![
            dc_member("local", "east", "http://local/", DcRole::Writer),
            dc_member("peer", "east", "http://peer/", DcRole::Replica),
        ],
        "local",
        SecretSource::File("/does/not/exist".into()),
    );

    assert!(receipt_sources(&config).is_err());
}

#[test]
fn test_receipt_sources_reject_an_unusable_peer_address() {
    let config = dc_config(
        vec![
            dc_member("local", "east", "http://local/", DcRole::Writer),
            dc_member("peer", "east", "not a url", DcRole::Replica),
        ],
        "local",
        SecretSource::Literal("t".to_owned()),
    );

    assert!(receipt_sources(&config).is_err());
}

#[test]
fn test_receipt_endpoint_router_is_absent_without_replication() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = peryx_storage::blob::BlobStorage::filesystem(dir.path().join("blobs"));

    assert!(receipt_endpoint_router(&Config::default(), &blobs).unwrap().is_none());
}

#[tokio::test]
async fn test_receipt_endpoint_router_serves_a_held_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = peryx_storage::blob::BlobStorage::filesystem(dir.path().join("blobs"));
    let digest = blobs.put_bytes(b"artifact bytes").await.unwrap();
    let config = dc_config(
        vec![dc_member("local", "east", "http://local/", DcRole::Writer)],
        "local",
        SecretSource::Literal("secret".to_owned()),
    );
    let router = receipt_endpoint_router(&config, &blobs).unwrap().unwrap();

    let response = router
        .oneshot(
            Request::get(format!("/+replication/v1/receipts/sha256/{}", digest.as_str()))
                .header(header::AUTHORIZATION, "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

fn ha_config(members: Vec<DcMember>, identity: &str, token: SecretSource) -> Config {
    Config {
        writer_identity: Some(identity.to_owned()),
        availability: AvailabilityConfig::Ha(ReplicationConfig::Primary {
            source: "ingress".to_owned(),
            token,
        }),
        dc_membership: Some(DcMembership {
            group: "group".to_owned(),
            members,
        }),
        ..Config::default()
    }
}

#[test]
fn test_remote_frontier_sources_are_empty_outside_ha() {
    let config = dc_config(
        vec![
            dc_member("local", "east", "http://local/", DcRole::Writer),
            dc_member("peer", "west", "http://peer/", DcRole::Replica),
        ],
        "local",
        SecretSource::Literal("t".to_owned()),
    );

    assert!(
        remote_frontier_sources(&config).unwrap().is_empty(),
        "a dc-mode node waits on no remote"
    );
}

#[test]
fn test_remote_frontier_sources_are_empty_for_a_single_datacenter_group() {
    let config = ha_config(
        vec![
            dc_member("local", "east", "http://local/", DcRole::Writer),
            dc_member("peer", "east", "http://peer/", DcRole::Replica),
        ],
        "local",
        SecretSource::Literal("t".to_owned()),
    );

    assert!(remote_frontier_sources(&config).unwrap().is_empty());
}

#[test]
fn test_remote_frontier_sources_cover_each_remote_datacenter_writer_preferred() {
    let config = ha_config(
        vec![
            dc_member("local", "east", "http://local/", DcRole::Writer),
            dc_member("west-replica", "west", "http://replica.west/", DcRole::Replica),
            dc_member("west-writer", "west", "http://writer.west/", DcRole::Writer),
            dc_member("south", "south", "http://south/", DcRole::Replica),
        ],
        "local",
        SecretSource::Literal("t".to_owned()),
    );
    let sources = remote_frontier_sources(&config).unwrap();

    let datacenters: std::collections::BTreeSet<String> =
        sources.iter().map(|source| source.datacenter().to_owned()).collect();
    assert_eq!(
        datacenters,
        std::collections::BTreeSet::from(["south".to_owned(), "west".to_owned()]),
        "one source per remote datacenter, and the local datacenter never appears",
    );
}

#[test]
fn test_remote_dc_roster_prefers_the_datacenter_writer_address() {
    let membership = DcMembership {
        group: "group".to_owned(),
        members: vec![
            dc_member("local", "east", "http://local/", DcRole::Writer),
            // west lists the replica first, then the writer replaces it.
            dc_member("west-replica", "west", "http://replica.west/", DcRole::Replica),
            dc_member("west-writer", "west", "http://writer.west/", DcRole::Writer),
            // south lists the writer first, so a later replica is skipped.
            dc_member("south-writer", "south", "http://writer.south/", DcRole::Writer),
            dc_member("south-replica", "south", "http://replica.south/", DcRole::Replica),
        ],
    };

    let roster = crate::server::remote_dc_roster(&membership, "east");

    assert_eq!(
        roster,
        std::collections::BTreeMap::from([
            ("south".to_owned(), "http://writer.south/".to_owned()),
            ("west".to_owned(), "http://writer.west/".to_owned()),
        ]),
        "the writer's address wins over a replica's in the same datacenter, whichever is rostered first",
    );
}

#[test]
fn test_remote_frontier_sources_surface_an_unreadable_token() {
    let config = ha_config(
        vec![
            dc_member("local", "east", "http://local/", DcRole::Writer),
            dc_member("peer", "west", "http://peer/", DcRole::Replica),
        ],
        "local",
        SecretSource::File("/does/not/exist".into()),
    );

    assert!(remote_frontier_sources(&config).is_err());
}

#[test]
fn test_remote_frontier_sources_reject_an_unusable_remote_address() {
    let config = ha_config(
        vec![
            dc_member("local", "east", "http://local/", DcRole::Writer),
            dc_member("peer", "west", "not a url", DcRole::Replica),
        ],
        "local",
        SecretSource::Literal("t".to_owned()),
    );

    assert!(remote_frontier_sources(&config).is_err());
}

#[test]
fn test_remote_frontier_sources_are_empty_without_a_roster() {
    let config = Config {
        writer_identity: Some("local".to_owned()),
        availability: AvailabilityConfig::Ha(ReplicationConfig::Primary {
            source: "ingress".to_owned(),
            token: SecretSource::Literal("t".to_owned()),
        }),
        ..Config::default()
    };

    assert!(remote_frontier_sources(&config).unwrap().is_empty());
}

#[test]
fn test_remote_frontier_sources_are_empty_when_the_node_is_absent_from_the_roster() {
    let config = ha_config(
        vec![
            dc_member("local", "east", "http://local/", DcRole::Writer),
            dc_member("peer", "west", "http://peer/", DcRole::Replica),
        ],
        "ghost",
        SecretSource::Literal("t".to_owned()),
    );

    assert!(remote_frontier_sources(&config).unwrap().is_empty());
}

#[test]
fn test_remote_frontier_sources_accept_a_scheme_less_address() {
    // A rostered `host:port` address with no scheme builds a usable transport rather than failing
    // startup, so a bare internal address does not panic build_state.
    let config = ha_config(
        vec![
            dc_member("local", "east", "http://local/", DcRole::Writer),
            dc_member("peer", "west", "10.0.0.2:8080", DcRole::Replica),
        ],
        "local",
        SecretSource::Literal("t".to_owned()),
    );

    let sources = remote_frontier_sources(&config).unwrap();

    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].datacenter(), "west");
}

#[test]
fn test_frontier_endpoint_router_is_absent_outside_ha() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    let state = build_state(&config).unwrap();

    assert!(frontier_endpoint_router(&config, &state).unwrap().is_none());
}

#[tokio::test]
async fn test_frontier_endpoint_router_serves_a_frontier() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some("local".to_owned()),
        availability: AvailabilityConfig::Ha(ReplicationConfig::Primary {
            source: "local".to_owned(),
            token: SecretSource::Literal("secret".to_owned()),
        }),
        dc_membership: Some(DcMembership {
            group: "group".to_owned(),
            members: vec![
                dc_member("local", "east", "http://local/", DcRole::Writer),
                dc_member("peer", "west", "http://peer/", DcRole::Replica),
            ],
        }),
        ..Config::default()
    };
    let state = build_state(&config).unwrap();
    let router = frontier_endpoint_router(&config, &state).unwrap().unwrap();

    let response = router
        .oneshot(
            Request::get("/+replication/v1/frontier/proj")
                .header(header::AUTHORIZATION, "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let reply: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(reply.get("applied_frontier").is_some(), "{reply}");
    assert!(reply.get("epoch").is_some(), "{reply}");
}
