use std::path::Path;

use axum::Router;
use axum::response::Redirect;
use axum::routing::get;
use peryx_core::Ecosystem;
use peryx_upstream::{Auth, RangeError, UpstreamClient, UpstreamError, UpstreamTls};
use rstest::rstest;

use super::tls_support::{TestPki, TlsFiles};
use crate::config::{Config, IndexConfig, IndexKind, UpstreamTlsConfig};
use crate::server::build_state;

fn cached_config(data_dir: &Path, ecosystem: Ecosystem, upstream: String, files: &TlsFiles) -> Config {
    Config {
        data_dir: data_dir.to_path_buf(),
        indexes: vec![IndexConfig {
            name: "private".to_owned(),
            route: "private".to_owned(),
            ecosystem,
            policy: peryx_policy::PolicyConfig::default(),
            ecosystem_policy: toml::Table::new(),
            ecosystem_settings: toml::Table::new(),
            anonymous_read: None,
            tokens: Vec::new(),
            webhooks: Vec::new(),
            kind: IndexKind::Cached {
                upstream,
                username: None,
                password: None,
                token: None,
                tls: UpstreamTlsConfig {
                    ca_file: Some(files.ca.clone()),
                    client_cert_file: Some(files.certificate.clone()),
                    client_key_file: Some(files.key.clone()),
                },
                routing: None,
                upstream_concurrency: 0,
                offline: false,
                prefetch: Box::default(),
            },
        }],
        ..Config::default()
    }
}

fn tls_client(base: &str, files: &TlsFiles) -> UpstreamClient {
    let tls = UpstreamTls::from_paths(Some(&files.ca), Some((&files.certificate, &files.key))).unwrap();
    UpstreamClient::with_auth_and_tls(base, Auth::None, &tls).unwrap()
}

fn ca_client(base: &str, files: &TlsFiles) -> UpstreamClient {
    let tls = UpstreamTls::from_paths(Some(&files.ca), None).unwrap();
    UpstreamClient::with_auth_and_tls(base, Auth::None, &tls).unwrap()
}

fn scoped_tls_client(base: &str, identity_origin: &str, files: &TlsFiles) -> UpstreamClient {
    let tls = UpstreamTls::from_paths(Some(&files.ca), Some((&files.certificate, &files.key))).unwrap();
    UpstreamClient::with_auth_and_tls_for_origin(base, Auth::None, &tls, identity_origin).unwrap()
}

fn ok_router() -> Router {
    Router::new().route("/ok", get(|| async { "ok" }))
}

#[derive(Debug, Clone, Copy)]
enum RequestKind {
    Artifact,
    Metadata,
}

impl RequestKind {
    async fn succeeds(self, client: &UpstreamClient, url: &str) -> bool {
        match self {
            Self::Artifact => client.fetch_bytes(url).await.is_ok(),
            Self::Metadata => matches!(
                client.head_file_for_range(url).await,
                Ok(_) | Err(RangeError::Unsupported)
            ),
        }
    }
}

#[rstest]
#[case::pypi_artifact(Ecosystem::Pypi, RequestKind::Artifact)]
#[case::pypi_metadata(Ecosystem::Pypi, RequestKind::Metadata)]
#[case::oci_artifact(Ecosystem::Oci, RequestKind::Artifact)]
#[case::oci_metadata(Ecosystem::Oci, RequestKind::Metadata)]
#[tokio::test]
async fn test_build_state_uses_upstream_ca_and_client_identity(
    #[case] ecosystem: Ecosystem,
    #[case] request: RequestKind,
) {
    let pki = TestPki::new();
    let files = pki.write_client_files();
    let server = pki.start_server(ok_router());
    let data = tempfile::tempdir().unwrap();
    let state = build_state(&cached_config(data.path(), ecosystem, server.url("/"), &files)).unwrap();

    assert!(
        request
            .succeeds(state.indexes[0].proxy_client().unwrap(), &server.url("/ok"))
            .await
    );
}

#[test]
fn test_build_state_rejects_unreadable_tls_before_serving() {
    let pki = TestPki::new();
    let mut files = pki.write_client_files();
    files.ca = files.ca.with_file_name("private-ca-secret.pem");
    let data = tempfile::tempdir().unwrap();
    let Err(error) = build_state(&cached_config(
        data.path(),
        Ecosystem::Pypi,
        "https://packages.example/".to_owned(),
        &files,
    )) else {
        panic!("expected invalid upstream TLS to fail state construction");
    };
    let message = format!("{error:#}");

    assert!(
        message.contains("load upstream TLS material for index private"),
        "{message}"
    );
    assert!(!message.contains("private-ca-secret"), "{message}");
}

#[test]
fn test_build_state_rejects_incomplete_programmatic_tls_identity() {
    let pki = TestPki::new();
    let files = pki.write_client_files();
    let data = tempfile::tempdir().unwrap();
    let mut config = cached_config(
        data.path(),
        Ecosystem::Pypi,
        "https://packages.example/".to_owned(),
        &files,
    );
    let IndexKind::Cached { tls, .. } = &mut config.indexes[0].kind else {
        panic!("expected cached index");
    };
    tls.client_key_file = None;
    let Err(error) = build_state(&config) else {
        panic!("expected incomplete upstream TLS identity to fail state construction");
    };

    assert!(error.to_string().contains("requires both upstream client certificate"));
}

#[tokio::test]
async fn test_custom_ca_does_not_require_a_client_identity() {
    let pki = TestPki::new();
    let files = pki.write_client_files();
    let server = pki.start_server_without_client_identity(ok_router());
    let response = ca_client(&server.url("/"), &files)
        .fetch_bytes(&server.url("/ok"))
        .await
        .unwrap();

    assert_eq!(response.as_ref(), b"ok");
}

#[rstest]
#[case::artifact(RequestKind::Artifact)]
#[case::metadata(RequestKind::Metadata)]
#[tokio::test]
async fn test_client_identity_is_not_offered_to_another_origin(#[case] request: RequestKind) {
    let pki = TestPki::new();
    let files = pki.write_client_files();
    let upstream = pki.start_server(ok_router());
    let other = pki.start_server(ok_router());
    let client = tls_client(&upstream.url("/"), &files);

    assert!(!request.succeeds(&client, &other.url("/ok")).await);
}

#[rstest]
#[case::artifact(RequestKind::Artifact)]
#[case::metadata(RequestKind::Metadata)]
#[tokio::test]
async fn test_client_identity_is_not_offered_to_configured_artifact_origin(#[case] request: RequestKind) {
    let pki = TestPki::new();
    let files = pki.write_client_files();
    let upstream = pki.start_server(ok_router());
    let artifact = pki.start_server(ok_router());

    let client = scoped_tls_client(&artifact.url("/"), &upstream.url("/"), &files);

    assert!(!request.succeeds(&client, &artifact.url("/ok")).await);
}

#[rstest]
#[case::artifact(RequestKind::Artifact)]
#[case::metadata(RequestKind::Metadata)]
#[tokio::test]
async fn test_configured_artifact_origin_keeps_custom_ca(#[case] request: RequestKind) {
    let pki = TestPki::new();
    let files = pki.write_client_files();
    let upstream = pki.start_server(ok_router());
    let artifact = pki.start_server_without_client_identity(ok_router());
    let client = scoped_tls_client(&artifact.url("/"), &upstream.url("/"), &files);

    assert!(request.succeeds(&client, &artifact.url("/ok")).await);
}

#[rstest]
#[case::artifact(RequestKind::Artifact)]
#[case::metadata(RequestKind::Metadata)]
#[tokio::test]
async fn test_custom_ca_applies_to_cross_origin_requests(#[case] request: RequestKind) {
    let pki = TestPki::new();
    let files = pki.write_client_files();
    let upstream = pki.start_server(ok_router());
    let artifact = pki.start_server_without_client_identity(ok_router());
    let client = tls_client(&upstream.url("/"), &files);

    assert!(request.succeeds(&client, &artifact.url("/ok")).await);
}

#[tokio::test]
async fn test_client_identity_rejects_cross_origin_redirects() {
    let pki = TestPki::new();
    let files = pki.write_client_files();
    let target = pki.start_server(ok_router());
    let target_url = target.url("/ok");
    let upstream = pki.start_server(Router::new().route(
        "/redirect",
        get(move || {
            let target_url = target_url.clone();
            async move { Redirect::temporary(&target_url) }
        }),
    ));
    let error = tls_client(&upstream.url("/"), &files)
        .fetch_bytes(&upstream.url("/redirect"))
        .await
        .unwrap_err();

    assert!(matches!(error, UpstreamError::Http(error) if error.is_redirect()));
}

#[tokio::test]
async fn test_client_identity_follows_same_origin_redirects() {
    let pki = TestPki::new();
    let files = pki.write_client_files();
    let upstream = pki.start_server(ok_router().route("/redirect", get(|| async { Redirect::temporary("/ok") })));

    assert_eq!(
        tls_client(&upstream.url("/"), &files)
            .fetch_bytes(&upstream.url("/redirect"))
            .await
            .unwrap()
            .as_ref(),
        b"ok"
    );
}

#[tokio::test]
async fn test_upstream_tls_requires_tls13() {
    let pki = TestPki::new();
    let files = pki.write_client_files();
    let server = pki.start_tls12_server(ok_router());

    assert!(
        tls_client(&server.url("/"), &files)
            .fetch_bytes(&server.url("/ok"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn test_rebuilding_state_reloads_rotated_tls_files() {
    let previous = TestPki::new();
    let replacement = TestPki::new();
    let unrelated = TestPki::new();
    let files = previous.write_client_files();
    let replacement_files = replacement.write_client_files();
    let unrelated_files = unrelated.write_client_files();
    let server = replacement.start_server(ok_router());
    let unrelated_server = unrelated.start_server(ok_router());
    let unrelated_client = tls_client(&unrelated_server.url("/"), &unrelated_files);
    let data = tempfile::tempdir().unwrap();
    let config = cached_config(data.path(), Ecosystem::Pypi, server.url("/"), &files);
    let previous_state = build_state(&config).unwrap();
    assert!(
        previous_state.indexes[0]
            .proxy_client()
            .unwrap()
            .fetch_bytes(&server.url("/ok"))
            .await
            .is_err()
    );
    drop(previous_state);

    std::fs::copy(&replacement_files.ca, &files.ca).unwrap();
    std::fs::copy(&replacement_files.certificate, &files.certificate).unwrap();
    std::fs::copy(&replacement_files.key, &files.key).unwrap();
    let replacement_state = build_state(&config).unwrap();

    assert_eq!(
        replacement_state.indexes[0]
            .proxy_client()
            .unwrap()
            .fetch_bytes(&server.url("/ok"))
            .await
            .unwrap()
            .as_ref(),
        b"ok"
    );
    assert_eq!(
        unrelated_client
            .fetch_bytes(&unrelated_server.url("/ok"))
            .await
            .unwrap()
            .as_ref(),
        b"ok"
    );
}
