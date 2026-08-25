use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use futures_util::TryStreamExt as _;
use http_body_util::BodyExt as _;
use peryx::config::{
    Config, CredentialFailureMode, CredentialRefreshConfig, IndexConfig, IndexKind, LogSink, SecretSource, TokenConfig,
    UpstreamConfig, UpstreamRoutingConfig, UpstreamTlsConfig, WebhookConfig, WebhookSecret,
};
use peryx::server::{build_router, build_state, check_config, router_for};
use peryx_driver::IndexKind as RuntimeKind;
use peryx_identity::{Action, GrantScope, Role};
use peryx_storage::meta::PolicyDecisionQuery;
use peryx_upstream::Auth;
#[cfg(unix)]
use peryx_upstream::{CredentialFailure, ExecCredentialConfig};
use rstest::rstest;
use tower::ServiceExt as _;
use wiremock::matchers::{header as match_header, header_regex, method, path};
use wiremock::{Match, Mock, MockServer, Request as WiremockRequest, ResponseTemplate};

fn config_with(dir: &tempfile::TempDir, indexes: Vec<IndexConfig>) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        indexes,
        ..Config::default()
    }
}

fn cached(name: &str, upstream: &str) -> IndexConfig {
    cached_from_routing(name, single_route(upstream))
}

fn cached_from_routing(name: &str, routing: UpstreamRoutingConfig) -> IndexConfig {
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
            routing,
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
            write_target: upload.map(str::to_owned),
        },
    }
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
fn routed(metadata: &str, artifact: Option<&str>) -> IndexConfig {
    let mut routing = single_route(metadata);
    routing.upstreams[0].artifact_url = artifact.map(str::to_owned);
    cached_from_routing("pypi", routing)
}

#[cfg(unix)]
fn exec_credential_helper(dir: &tempfile::TempDir) -> (ExecCredentialConfig, PathBuf, PathBuf) {
    let executions = dir.path().join("credential-executions");
    let requests = dir.path().join("credential-requests");
    (
        ExecCredentialConfig::new(
            vec![
                peryx_test_support::cargo_binary("peryx-pypi-credential-fixture")
                    .display()
                    .to_string(),
                requests.display().to_string(),
                executions.display().to_string(),
            ],
            Duration::from_mins(1),
            vec!["LLVM_PROFILE_FILE".to_owned()],
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
    let user = state.serving.users.create("Alice").unwrap();
    state
        .serving
        .users
        .set_password(&user.id, "local password")
        .await
        .unwrap();
    state
        .serving
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
    let partial = peryx::config::from_toml(
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
    let client = state.serving.indexes[0].proxy_client().unwrap();

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
    let source = state.serving.upstream_routes["pypi"].source("primary").unwrap();

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
    let mut routing = single_route(&format!("{}/simple/", metadata.uri()));
    routing.upstreams[0].artifact_url = Some(format!("{}/packages/", artifacts.uri()));
    routing.upstreams[0].token = Some(SecretSource::File(token.clone()));
    routing.upstreams[0].credential_refresh = Some(CredentialRefreshConfig {
        interval: Duration::from_hours(1),
        on_unauthorized: true,
        failure: CredentialFailureMode::Fail,
    });
    let index = cached_from_routing("pypi", routing);
    let state = build_state(&Config {
        data_dir: dir.path().join("data"),
        indexes: vec![index],
        ..Config::default()
    })
    .unwrap();
    std::fs::write(token, "new\n").unwrap();
    let source = state.serving.upstream_routes["pypi"].source("primary").unwrap();

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
    let mut routing = single_route(&format!("{}/simple/", upstream.uri()));
    routing.upstreams[0].credential_exec = Some(exec);
    let index = cached_from_routing("corp", routing);
    let state = build_state(&Config {
        data_dir: dir.path().join("data"),
        indexes: vec![index],
        ..Config::default()
    })
    .unwrap();
    let client = state.serving.indexes[0].proxy_client().unwrap();

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
    let mut routing = single_route(&format!("{}/simple/", metadata.uri()));
    routing.upstreams[0].artifact_url = Some(format!("{}/packages/", artifacts.uri()));
    routing.upstreams[0].credential_exec = Some(exec);
    let index = cached_from_routing("pypi", routing);
    let state = build_state(&Config {
        data_dir: dir.path().join("data"),
        indexes: vec![index],
        ..Config::default()
    })
    .unwrap();
    let source = state.serving.upstream_routes["pypi"].source("primary").unwrap();

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
    let mut routing = single_route(&format!("{}/simple/", upstream.uri()));
    routing.upstreams[0].token = Some(SecretSource::File(token.clone()));
    routing.upstreams[0].credential_refresh = Some(CredentialRefreshConfig {
        interval: Duration::from_hours(1),
        on_unauthorized: true,
        failure: CredentialFailureMode::Anonymous,
    });
    let index = cached_from_routing("corp", routing);
    let state = build_state(&Config {
        data_dir: dir.path().join("data"),
        indexes: vec![index],
        ..Config::default()
    })
    .unwrap();
    std::fs::remove_file(token).unwrap();
    let client = state.serving.indexes[0].proxy_client().unwrap();

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
    let mut routing = single_route("https://corp.example/simple/");
    routing.upstreams[0].username = username.map(str::to_owned);
    routing.upstreams[0].password = password;
    routing.upstreams[0].token = token;
    let index = cached_from_routing("corp", routing);
    let state = build_state(&Config {
        data_dir: dir.path().join("data"),
        netrc: Some(netrc),
        indexes: vec![index],
        ..Config::default()
    })
    .unwrap();
    let client = state.serving.indexes[0].proxy_client().unwrap();

    assert_eq!(current_auth(client), expected);
}

#[test]
fn test_build_state_reads_an_upstream_token_from_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    let mut routing = single_route("https://corp.example/simple/");
    routing.upstreams[0].token = Some(SecretSource::Env("PATH".to_owned()));
    let index = cached_from_routing("corp", routing);
    let state = build_state(&Config {
        data_dir: dir.path().join("data"),
        indexes: vec![index],
        ..Config::default()
    })
    .unwrap();
    let client = state.serving.indexes[0].proxy_client().unwrap();

    assert_eq!(
        current_auth(client),
        Auth::Bearer(std::env::var("PATH").unwrap().trim().to_owned())
    );
}

#[test]
fn test_build_state_reports_a_missing_upstream_credential_environment_variable() {
    let dir = tempfile::tempdir().unwrap();
    let mut routing = single_route("https://corp.example/simple/");
    routing.upstreams[0].token = Some(SecretSource::Env("PERYX_TEST_ABSENT_CREDENTIAL".to_owned()));
    let error = build_state(&Config {
        data_dir: dir.path().join("data"),
        indexes: vec![cached_from_routing("corp", routing)],
        ..Config::default()
    })
    .err()
    .expect("missing credential environment variable fails startup");
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
    let error = build_state(&Config {
        data_dir: dir.path().join("data"),
        indexes: vec![hosted("store"), virtual_index(&["ghost"], None)],
        ..Config::default()
    })
    .err()
    .expect("unknown virtual layer fails startup");

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
    let client = state.serving.indexes[0].proxy_client().unwrap();

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
    let error = build_state(&Config {
        data_dir: dir.path().join("data"),
        netrc: Some(netrc),
        indexes: vec![cached("corp", "https://corp.example/simple/")],
        ..Config::default()
    })
    .err()
    .expect("invalid netrc syntax fails startup");
    let message = format!("{error:#}");

    assert!(message.contains("load upstream netrc"));
    assert!(message.contains("has invalid syntax"));
    assert!(!message.contains("swordfish"));
}

#[test]
fn test_build_state_reports_an_unreadable_netrc_path() {
    let dir = tempfile::tempdir().unwrap();
    let netrc = dir.path().join("missing.netrc");
    let error = build_state(&Config {
        data_dir: dir.path().join("data"),
        netrc: Some(netrc.clone()),
        indexes: vec![cached("corp", "https://corp.example/simple/")],
        ..Config::default()
    })
    .err()
    .expect("missing netrc fails startup");
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
    let error = build_state(&Config {
        data_dir: dir.path().join("data"),
        netrc: Some(netrc),
        indexes: vec![cached("corp", "https://corp.example/simple/")],
        ..Config::default()
    })
    .err()
    .expect("insecure netrc mode fails startup");
    let message = format!("{error:#}");

    assert!(message.contains("must not grant group or other permissions"));
    assert!(!message.contains("swordfish"));
}

#[cfg(unix)]
#[test]
fn test_build_state_rejects_a_netrc_owned_by_another_user() {
    let dir = tempfile::tempdir().unwrap();
    let error = build_state(&Config {
        data_dir: dir.path().join("data"),
        netrc: Some(PathBuf::from("/etc/hosts")),
        indexes: vec![cached("corp", "https://corp.example/simple/")],
        ..Config::default()
    })
    .err()
    .expect("foreign netrc owner fails startup");

    assert!(format!("{error:#}").contains("must be owned by the effective user"));
}

#[test]
fn test_build_state_records_policy_decisions() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = hosted("private");
    index.policy.block_resources = vec!["blocked".to_owned()];
    let state = build_state(&config_with(&dir, vec![index])).unwrap();

    state.serving.indexes[0]
        .policy
        .check_facts(
            peryx_policy::PolicyAction::Serve,
            &peryx_policy::ArtifactFacts {
                resource: "blocked".to_owned(),
                artifact: Some("blocked-1.0.whl".to_owned()),
                group: Some("1.0".to_owned()),
                source: Some("pypi".to_owned()),
                ..peryx_policy::ArtifactFacts::default()
            },
        )
        .unwrap_err();
    let mut record = serde_json::to_value(
        &state
            .serving
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
            "resource": "blocked",
            "group": "1.0",
            "artifact": "blocked-1.0.whl",
            "source": "pypi",
            "action": "serve",
            "state": "deny",
            "rule": "resource-block-list",
            "reason": "resource \"blocked\" is blocked",
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
            state.serving.indexes[0]
                .policy
                .check_resource(peryx_policy::PolicyAction::Serve, &"x".repeat(513)),
            state
                .serving
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
fn test_build_state_makes_replica_upstreams_offline() {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&Config {
        data_dir: dir.path().to_path_buf(),
        read_only: true,
        ..Config::default()
    })
    .unwrap();
    assert!(state.serving.read_only);
    assert!(state.serving.indexes.iter().all(|index| match &index.kind {
        peryx_driver::IndexKind::Cached { offline, .. } => *offline,
        peryx_driver::IndexKind::Hosted { .. } | peryx_driver::IndexKind::Virtual { .. } => true,
    }));
}

#[rstest]
#[case::read_only(false)]
fn test_build_state_applies_upstream_concurrency(#[case] read_only: bool) {
    let dir = tempfile::tempdir().unwrap();
    let mut pypi = cached("pypi", "https://pypi.org/simple/");
    pypi.kind = IndexKind::Cached {
        routing: single_route("https://pypi.org/simple/"),
        upstream_concurrency: 2,
        offline: false,
        prefetch: Box::default(),
    };
    let config = Config {
        read_only,
        ..config_with(&dir, vec![pypi])
    };

    let state = build_state(&config).unwrap();

    let snapshots = state.serving.upstream_limits.snapshots();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].max_concurrent, 2);
}

#[test]
fn test_build_state_reports_index_errors() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with(&dir, vec![cached("pypi", "not a url")]);

    let err = build_state(&config).err().expect("invalid index fails startup");

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

    let err = build_state(&config).err().expect("invalid webhook fails startup");

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

    let err = build_state(&config)
        .err()
        .expect("missing webhook secret fails startup");

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

    assert!(!state.serving.webhooks.is_empty());
}

#[rstest]
#[case::artifact("https://metadata.example/simple/", Some("not a url"))]
fn test_build_state_rejects_invalid_routed_source_urls(#[case] metadata: &str, #[case] artifact: Option<&str>) {
    let dir = tempfile::tempdir().unwrap();
    let netrc = dir.path().join("credentials.netrc");
    write_netrc(&netrc, "default login reader password swordfish\n");

    let err = build_state(&Config {
        netrc: Some(netrc),
        ..config_with(&dir, vec![routed(metadata, artifact)])
    })
    .err()
    .expect("invalid routed source fails startup");

    let message = format!("{err:#}");
    assert!(message.contains("match netrc credentials for <invalid upstream URL>"));
    assert!(!message.contains("swordfish"));
}

#[test]
fn test_build_state_rejects_invalid_routed_metadata_without_netrc() {
    let dir = tempfile::tempdir().unwrap();
    let error = build_state(&config_with(&dir, vec![routed("not a url", None)]))
        .err()
        .expect("invalid routed source fails startup");

    let message = format!("{error:#}");
    assert!(message.contains("build cached index pypi with upstream <invalid upstream URL>"));
}

#[rstest]
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
#[case::invalid_policy_type(
    || {
        let mut index = cached("pypi", "https://pypi.org/simple/");
        index.ecosystem_policy.insert("allow_projects".to_owned(), 1.into());
        vec![index]
    },
    &["compile policy for pypi"][..]
)]
fn test_build_state_rejects_pypi_policy(#[case] indexes: fn() -> Vec<IndexConfig>, #[case] expected: &[&str]) {
    let dir = tempfile::tempdir().unwrap();
    let err = build_state(&config_with(&dir, indexes()))
        .err()
        .expect("invalid policy fails startup");
    let message = err.to_string();
    for substr in expected {
        assert!(message.contains(substr), "{message}");
    }
}

#[test]
fn test_build_index_settings_surfaces_plugin_errors() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = cached("cache", "https://packages.example/");
    index.ecosystem_settings.insert("unknown".to_owned(), true.into());
    let error = check_config(&config_with(&dir, vec![index])).unwrap_err();
    let message = format!("{error:#}");
    assert_eq!(
        message,
        "compile ecosystem settings for cache: compile settings for cache: unknown field `unknown` in `[index.settings]`"
    );
}

#[test]
fn test_build_indexes_reports_an_unreadable_secret_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = hosted("store");
    index.tokens = vec![writer_token(SecretSource::File(PathBuf::from(
        "/nonexistent/peryx/token",
    )))];

    let err = build_state(&config_with(&dir, vec![index]))
        .err()
        .expect("unreadable index secret fails startup");

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
    let mut routing = single_route("https://corp/simple/");
    routing.upstreams[0].password = Some(SecretSource::File(password));
    routing.upstreams[0].token = Some(SecretSource::File(token));

    let state = build_state(&config_with(&dir, vec![cached_from_routing("corp", routing)])).unwrap();

    assert!(matches!(&state.serving.indexes[0].kind, RuntimeKind::Cached { .. }));
}

#[test]
fn test_build_state_installs_normalized_upstream_routes() {
    let dir = tempfile::tempdir().unwrap();
    let partial = peryx::config::from_toml(
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
    let router = &state.serving.upstream_routes["pypi"];

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
    let dir = tempfile::tempdir().unwrap();
    let mut routing = single_route("https://corp/simple/");
    routing.upstreams[0].password = Some(SecretSource::File(PathBuf::from("/nonexistent/peryx/password")));

    let err = build_state(&config_with(&dir, vec![cached_from_routing("corp", routing)]))
        .err()
        .expect("unreadable upstream credential fails startup");

    assert!(
        err.to_string().contains("read the upstream credentials of index corp"),
        "{err}"
    );
}

#[test]
fn test_build_indexes_defaults_write_target_to_first_local_layer() {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&config_with(
        &dir,
        vec![
            cached("pypi", "https://pypi.org/simple/"),
            hosted("store"),
            virtual_index(&["pypi", "store"], None),
        ],
    ))
    .unwrap();
    assert!(matches!(
        &state.serving.indexes[2].kind,
        RuntimeKind::Virtual { write_target: Some(1), layers } if layers == &[0, 1]
    ));
}

#[test]
fn test_build_indexes_overlay_without_local_layer_has_no_write_target() {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&config_with(
        &dir,
        vec![
            cached("pypi", "https://pypi.org/simple/"),
            virtual_index(&["pypi"], None),
        ],
    ))
    .unwrap();
    assert!(matches!(
        &state.serving.indexes[1].kind,
        RuntimeKind::Virtual { write_target: None, .. }
    ));
}

fn single_route(url: &str) -> UpstreamRoutingConfig {
    UpstreamRoutingConfig {
        upstreams: vec![UpstreamConfig {
            name: "primary".to_owned(),
            url: url.to_owned(),
            artifact_url: None,
            username: None,
            password: None,
            token: None,
            credential_exec: None,
            credential_refresh: None,
            tls: UpstreamTlsConfig::default(),
        }],
        fallback: true,
        protected: Vec::new(),
        pins: BTreeMap::new(),
    }
}

fn writer_token(secret: SecretSource) -> TokenConfig {
    TokenConfig {
        name: "uploader".to_owned(),
        secret,
        resources: vec!["*".to_owned()],
        actions: BTreeSet::from([Action::Write, Action::Delete]),
        expires_at: None,
    }
}
