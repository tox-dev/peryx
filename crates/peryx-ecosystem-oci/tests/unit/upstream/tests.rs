use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use peryx_upstream::{CredentialFailure, CredentialProvider, CredentialRefresh, UpstreamClient, UpstreamTls};
use tokio::io::AsyncReadExt as _;
use tokio::sync::Barrier;

use super::*;
use rstest::rstest;

fn header(value: &str) -> HeaderValue {
    HeaderValue::from_str(value).unwrap()
}

#[test]
fn test_parse_bearer_reads_realm_service_scope() {
    let challenge = parse_bearer(&header(
        r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/nginx:pull""#,
    ))
    .unwrap();
    assert_eq!(challenge.realm, "https://auth.docker.io/token");
    assert_eq!(challenge.service.as_deref(), Some("registry.docker.io"));
    assert_eq!(challenge.scope.as_deref(), Some("repository:library/nginx:pull"));
}

#[test]
fn test_parse_bearer_realm_only() {
    let challenge = parse_bearer(&header(r#"bearer realm="https://auth.example/token""#)).unwrap();
    assert_eq!(challenge.realm, "https://auth.example/token");
    assert_eq!(challenge.service, None);
    assert_eq!(challenge.scope, None);
}

#[test]
fn test_parse_bearer_rejects_non_bearer_scheme() {
    assert_eq!(parse_bearer(&header(r#"Basic realm="x""#)), None);
}

#[test]
fn test_parse_bearer_requires_a_realm() {
    assert_eq!(parse_bearer(&header(r#"Bearer service="registry.docker.io""#)), None);
}

#[test]
fn test_parse_bearer_rejects_malformed_parameter() {
    assert_eq!(parse_bearer(&header("Bearer realmnoeq")), None);
}

#[test]
fn test_parse_bearer_ignores_unknown_parameters() {
    let challenge = parse_bearer(&header(
        r#"Bearer realm="https://auth.example/token",error="insufficient_scope""#,
    ))
    .unwrap();
    assert_eq!(challenge.realm, "https://auth.example/token");
}

#[test]
fn test_upstream_error_display() {
    assert_eq!(
        UpstreamError::Status(StatusCode::NOT_FOUND).to_string(),
        "upstream returned 404 Not Found"
    );
    assert_eq!(UpstreamError::Transport("reset".to_owned()).to_string(), "reset");
    assert_eq!(
        UpstreamError::RateLimited(Some("5".to_owned())).to_string(),
        "upstream rate limit reached"
    );
}

fn basic(username: &str, password: &str) -> Auth {
    Auth::Basic {
        username: username.to_owned(),
        password: password.to_owned(),
    }
}

fn credentials(auth: Auth) -> CredentialProvider {
    CredentialProvider::fixed(auth)
}

fn basic_header(username: &str, password: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"))
    )
}

fn configured_realms(origins: &[&str]) -> TokenRealms {
    let entries = origins.iter().map(|origin| toml::Value::from(*origin)).collect();
    TokenRealms::parse(&toml::Value::Array(entries)).unwrap()
}

fn upstream_client(base: &str, credentials: CredentialProvider) -> UpstreamClient {
    UpstreamClient::with_credentials_and_tls_for_origin(base, credentials, &UpstreamTls::default(), base, &[]).unwrap()
}

use base64::Engine as _;
use wiremock::matchers::{header as match_header, method, path, query_param};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

use crate::tests::{ResponseGate, gated_response, response_gate};

/// The matcher leaves the authenticated retry for the success mock.
struct Unauthenticated;
impl Match for Unauthenticated {
    fn matches(&self, request: &Request) -> bool {
        !request.headers.contains_key("authorization")
    }
}

fn challenge(base: &str) -> ResponseTemplate {
    ResponseTemplate::new(401).insert_header(
        "www-authenticate",
        format!(r#"Bearer realm="{base}token",service=reg,scope="repository:library/nginx:pull""#).as_str(),
    )
}

async fn assert_manifest_authenticates(server: &MockServer, base: &str) {
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"token":"tok"}"#))
        .expect(1)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(match_header("authorization", "Bearer tok"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(server)
        .await;

    let response = Upstream::new()
        .manifest(
            &upstream_client(base, credentials(Auth::None)),
            "library/nginx",
            "latest",
            &TokenRealms::default(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_manifest_selects_bearer_from_combined_challenges() {
    let server = MockServer::start().await;
    let base = format!("{}/", server.uri());
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(Unauthenticated)
        .respond_with(ResponseTemplate::new(401).insert_header(
            "www-authenticate",
            format!(
                r#"Basic realm="login", bEaReR ReAlM="{base}token?aud=a,b",SeRvIcE="reg\"istry",ScOpE="repository:library\/nginx:pull","#
            )
            .as_str(),
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(query_param("aud", "a,b"))
        .and(query_param("service", "reg\"istry"))
        .and(query_param("scope", "repository:library/nginx:pull"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"token":"tok"}"#))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(match_header("authorization", "Bearer tok"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let response = Upstream::new()
        .manifest(
            &upstream_client(&base, credentials(Auth::None)),
            "library/nginx",
            "latest",
            &TokenRealms::default(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_manifest_selects_bearer_from_repeated_challenge_fields() {
    let server = MockServer::start().await;
    let base = format!("{}/", server.uri());
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(Unauthenticated)
        .respond_with(
            ResponseTemplate::new(401)
                .append_header("www-authenticate", r#"Basic realm="login"#)
                .append_header("www-authenticate", format!(r#"Bearer realm="{base}token""#).as_str()),
        )
        .expect(1)
        .mount(&server)
        .await;

    assert_manifest_authenticates(&server, &base).await;
}

#[tokio::test]
async fn test_manifest_selects_bearer_after_malformed_challenge() {
    let server = MockServer::start().await;
    let base = format!("{}/", server.uri());
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(Unauthenticated)
        .respond_with(ResponseTemplate::new(401).insert_header(
            "www-authenticate",
            format!(r#"Bearer realmnoeq, Bearer realm="{base}token""#).as_str(),
        ))
        .expect(1)
        .mount(&server)
        .await;

    assert_manifest_authenticates(&server, &base).await;
}

#[tokio::test]
async fn test_fetch_token_rejects_an_oversized_response() {
    let server = MockServer::start().await;
    let base = format!("{}/", server.uri());
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(Unauthenticated)
        .respond_with(challenge(&base))
        .expect(1)
        .mount(&server)
        .await;
    let oversized = format!(r#"{{"filler":"{}"}}"#, "A".repeat(2 * 1024 * 1024));
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(oversized))
        .mount(&server)
        .await;

    let error = Upstream::new()
        .manifest(
            &upstream_client(&base, credentials(Auth::None)),
            "library/nginx",
            "latest",
            &TokenRealms::default(),
        )
        .await
        .unwrap_err();

    assert!(error.to_string().contains("exceeds"), "{error}");
}

#[rstest]
#[case::realm_exact_token("realm=invalid")]
#[case::realm_mixed_quoted(r#"ReAlM="://""#)]
#[case::service_exact_equal_token("service=registry,service=registry")]
#[case::service_mixed_different_quoted(r#"service="first",SeRvIcE="second""#)]
#[case::scope_exact_equal_quoted(r#"scope="repository:app:pull",scope="repository:app:pull""#)]
#[case::scope_mixed_different_token("scope=first,ScOpE=second")]
#[case::extension_exact_equal_quoted(r#"extension="value",extension="value""#)]
#[case::extension_mixed_different_token("extension=first,ExTeNsIoN=second")]
#[tokio::test]
async fn test_manifest_skips_bearer_with_duplicate_parameter(#[case] parameters: &str) {
    let server = MockServer::start().await;
    let base = format!("{}/", server.uri());
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(Unauthenticated)
        .respond_with(ResponseTemplate::new(401).insert_header(
            "www-authenticate",
            format!(r#"Bearer realm="{base}wrong",{parameters}, Bearer realm="{base}token""#).as_str(),
        ))
        .expect(1)
        .mount(&server)
        .await;

    assert_manifest_authenticates(&server, &base).await;
}

#[tokio::test]
async fn test_manifest_selects_bearer_after_duplicate_parameter_field() {
    let server = MockServer::start().await;
    let base = format!("{}/", server.uri());
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(Unauthenticated)
        .respond_with(
            ResponseTemplate::new(401)
                .append_header(
                    "www-authenticate",
                    format!(r#"Bearer realm="{base}wrong",realm=invalid"#).as_str(),
                )
                .append_header("www-authenticate", format!(r#"Bearer realm="{base}token""#).as_str()),
        )
        .expect(1)
        .mount(&server)
        .await;

    assert_manifest_authenticates(&server, &base).await;
}

#[rstest]
#[case::missing_name("Bearer =token")]
#[case::unterminated_quote(r#"Bearer realm="https://auth.example/token"#)]
#[case::unterminated_escape(r#"Bearer realm="https://auth.example/token\"#)]
#[case::unterminated_after_escape(r#"Bearer realm="https://auth.example/\token"#)]
#[tokio::test]
async fn test_manifest_rejects_malformed_bearer_parameters(#[case] challenge: &str) {
    let server = MockServer::start().await;
    let base = format!("{}/", server.uri());
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(401).insert_header("www-authenticate", challenge))
        .expect(1)
        .mount(&server)
        .await;

    let result = Upstream::new()
        .manifest(
            &upstream_client(&base, credentials(Auth::None)),
            "library/nginx",
            "latest",
            &TokenRealms::default(),
        )
        .await;

    assert!(matches!(result, Err(UpstreamError::Status(StatusCode::UNAUTHORIZED))));
}

#[tokio::test]
async fn test_manifest_rejects_an_invalid_bearer_realm() {
    let server = MockServer::start().await;
    let base = format!("{}/", server.uri());
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(401).insert_header("www-authenticate", r#"Bearer realm="://""#))
        .expect(1)
        .mount(&server)
        .await;

    let result = Upstream::new()
        .manifest(
            &upstream_client(&base, credentials(Auth::None)),
            "library/nginx",
            "latest",
            &TokenRealms::default(),
        )
        .await;

    assert!(matches!(result, Err(UpstreamError::Transport(message)) if message.starts_with("invalid bearer realm:")));
}

#[tokio::test]
async fn test_manifest_blocks_a_private_bearer_realm() {
    let server = MockServer::start().await;
    let base = format!("{}/", server.uri());
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(
            ResponseTemplate::new(401)
                .insert_header("www-authenticate", r#"Bearer realm="http://169.254.169.254/token""#),
        )
        .expect(1)
        .mount(&server)
        .await;

    let error = Upstream::new()
        .manifest(
            &upstream_client(&base, credentials(Auth::None)),
            "library/nginx",
            "latest",
            &TokenRealms::default(),
        )
        .await
        .unwrap_err();

    assert!(
        error.to_string().contains("169.254.169.254 is not a public address"),
        "{error}"
    );
}

#[rstest]
#[case::loopback_hostname(
    "http://localhost:9/private",
    "host resolves only to non-public addresses; configure `trusted_hosts` to allow it"
)]
#[case::link_local_literal("http://169.254.169.254/private", "169.254.169.254 is not a public address")]
#[case::private_literal("http://10.0.0.1/private", "10.0.0.1 is not a public address")]
#[tokio::test]
async fn test_manifest_blocks_redirects_to_private_destinations(#[case] location: &str, #[case] reason: &str) {
    let server = MockServer::start().await;
    let base = format!("{}/", server.uri());
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", location))
        .expect(1)
        .mount(&server)
        .await;

    let error = Upstream::new()
        .manifest(
            &upstream_client(&base, credentials(Auth::None)),
            "library/nginx",
            "latest",
            &TokenRealms::default(),
        )
        .await
        .unwrap_err();

    assert!(error.to_string().contains(reason), "{error}");
}

#[tokio::test]
async fn test_manifest_has_the_shared_read_deadline() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/", listener.local_addr().unwrap());
    let client = upstream_client(&base, credentials(Auth::None));
    let (request_seen_tx, request_seen_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut connection, _) = listener.accept().await.unwrap();
        let mut request_bytes = [0; 4_096];
        let received = connection.read(&mut request_bytes).await.unwrap();
        assert!(request_bytes[..received].starts_with(b"GET /v2/library/nginx/manifests/latest"));
        request_seen_tx.send(()).unwrap();
        let _ = release_rx.await;
    });
    let request = tokio::spawn(async move {
        Upstream::new()
            .manifest(&client, "library/nginx", "latest", &TokenRealms::default())
            .await
    });
    request_seen_rx.await.unwrap();
    tokio::time::pause();
    assert!(!request.is_finished());
    tokio::time::advance(Duration::from_secs(31)).await;

    assert!(matches!(request.await.unwrap(), Err(UpstreamError::Transport(_))));
    release_tx.send(()).unwrap();
    server.await.unwrap();
}

/// A realm the operator did not name receives the token request but not the secret.
async fn assert_token_request_is_anonymous(server: &MockServer, auth: &MockServer, client: &UpstreamClient) {
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(Unauthenticated)
        .respond_with(challenge(&format!("{}/", auth.uri())))
        .expect(1)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(Unauthenticated)
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"token":"tok"}"#))
        .expect(1)
        .mount(auth)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(match_header("authorization", "Bearer tok"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(server)
        .await;

    let response = Upstream::new()
        .manifest(client, "library/nginx", "latest", &TokenRealms::default())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[rstest]
#[case::basic(Auth::Basic { username: "alice".to_owned(), password: "pw".to_owned() })]
#[case::bearer(Auth::Bearer("configured".to_owned()))]
#[tokio::test]
async fn test_fetch_token_withholds_credentials_from_an_untrusted_realm_origin(#[case] auth: Auth) {
    let server = MockServer::start().await;
    let realm = MockServer::start().await;
    let client = upstream_client(&format!("{}/", server.uri()), credentials(auth));

    assert_token_request_is_anonymous(&server, &realm, &client).await;
}

#[tokio::test]
async fn test_fetch_token_sends_basic_credentials_to_the_upstream_origin() {
    let server = MockServer::start().await;
    let base = format!("{}/", server.uri());
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(Unauthenticated)
        .respond_with(challenge(&base))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(match_header("authorization", basic_header("alice", "pw").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"token":"tok"}"#))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(match_header("authorization", "Bearer tok"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    // The upstream is reached over cleartext here, which is the case a `localhost` registry serves.
    let response = Upstream::new()
        .manifest(
            &upstream_client(&base, credentials(basic("alice", "pw"))),
            "library/nginx",
            "latest",
            &TokenRealms::default(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_fetch_token_sends_basic_credentials_to_a_configured_realm_origin() {
    let server = MockServer::start().await;
    let realm = MockServer::start().await;
    let base = format!("{}/", server.uri());
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(Unauthenticated)
        .respond_with(challenge(&format!("{}/", realm.uri())))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(match_header("authorization", basic_header("alice", "pw").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"token":"tok"}"#))
        .expect(1)
        .mount(&realm)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(match_header("authorization", "Bearer tok"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    // Docker Hub's shape: the authorization service answers on an origin of its own.
    let response = Upstream::new()
        .manifest(
            &upstream_client(&base, credentials(basic("alice", "pw"))),
            "library/nginx",
            "latest",
            &configured_realms(&[&realm.uri()]),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_fetch_token_names_the_untrusted_realm_origin_it_withheld_from() {
    let server = MockServer::start().await;
    let realm = MockServer::start().await;
    let base = format!("{}/", server.uri());
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(Unauthenticated)
        .respond_with(challenge(&format!("{}/", realm.uri())))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(Unauthenticated)
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&realm)
        .await;

    let error = Upstream::new()
        .manifest(
            &upstream_client(&base, credentials(basic("alice", "pw"))),
            "library/nginx",
            "latest",
            &TokenRealms::default(),
        )
        .await
        .unwrap_err();

    // Only the origin, so the message carries neither the realm path nor the requested scope.
    assert_eq!(
        error.to_string(),
        format!(
            "bearer realm {} is not a trusted token realm for this upstream, so the token request \
             carried no credentials; add it to `token_realms` to authenticate there",
            realm.uri()
        )
    );
}

#[tokio::test]
async fn test_fetch_token_drops_credentials_on_a_redirect_to_another_origin() {
    let server = MockServer::start().await;
    let realm = MockServer::start().await;
    let base = format!("{}/", server.uri());
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(Unauthenticated)
        .respond_with(challenge(&base))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(match_header("authorization", basic_header("alice", "pw").as_str()))
        .respond_with(ResponseTemplate::new(302).insert_header("location", format!("{}/token", realm.uri()).as_str()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(Unauthenticated)
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"token":"tok"}"#))
        .expect(1)
        .mount(&realm)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(match_header("authorization", "Bearer tok"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let response = Upstream::new()
        .manifest(
            &upstream_client(&base, credentials(basic("alice", "pw"))),
            "library/nginx",
            "latest",
            &TokenRealms::default(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_fetch_token_keeps_credentials_across_a_redirect_within_the_trusted_origin() {
    let server = MockServer::start().await;
    let base = format!("{}/", server.uri());
    let credential = basic_header("alice", "pw");
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(Unauthenticated)
        .respond_with(challenge(&base))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(match_header("authorization", credential.as_str()))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/auth/token"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/auth/token"))
        .and(match_header("authorization", credential.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"token":"tok"}"#))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(match_header("authorization", "Bearer tok"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let response = Upstream::new()
        .manifest(
            &upstream_client(&base, credentials(basic("alice", "pw"))),
            "library/nginx",
            "latest",
            &TokenRealms::default(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[rstest]
#[case::private_literal("http://169.254.169.254/token", "169.254.169.254 is not a public address")]
#[case::resolves_to_loopback(
    "http://localhost:9/token",
    "host resolves only to non-public addresses; configure `trusted_hosts` to allow it"
)]
#[case::unparseable("http://[", "invalid bearer realm redirect")]
#[tokio::test]
async fn test_fetch_token_rejects_a_realm_redirect_off_the_public_internet(
    #[case] location: &str,
    #[case] reason: &str,
) {
    let server = MockServer::start().await;
    let base = format!("{}/", server.uri());
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(Unauthenticated)
        .respond_with(challenge(&base))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", location))
        .expect(1)
        .mount(&server)
        .await;

    let error = Upstream::new()
        .manifest(
            &upstream_client(&base, credentials(basic("alice", "pw"))),
            "library/nginx",
            "latest",
            &TokenRealms::default(),
        )
        .await
        .unwrap_err();

    assert!(error.to_string().contains(reason), "{error}");
}

#[tokio::test]
async fn test_fetch_token_stops_a_realm_that_keeps_redirecting() {
    let server = MockServer::start().await;
    let base = format!("{}/", server.uri());
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(Unauthenticated)
        .respond_with(challenge(&base))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(307).insert_header("location", "/token"))
        .expect(4)
        .mount(&server)
        .await;

    let error = Upstream::new()
        .manifest(
            &upstream_client(&base, credentials(Auth::None)),
            "library/nginx",
            "latest",
            &TokenRealms::default(),
        )
        .await
        .unwrap_err();

    assert_eq!(error.to_string(), "bearer realm redirected more than 3 times");
}

#[tokio::test]
async fn test_fetch_token_reports_a_redirect_without_a_location() {
    let server = MockServer::start().await;
    let base = format!("{}/", server.uri());
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(Unauthenticated)
        .respond_with(challenge(&base))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(302))
        .expect(1)
        .mount(&server)
        .await;

    let result = Upstream::new()
        .manifest(
            &upstream_client(&base, credentials(Auth::None)),
            "library/nginx",
            "latest",
            &TokenRealms::default(),
        )
        .await;

    assert!(matches!(result, Err(UpstreamError::Status(StatusCode::FOUND))));
}

#[tokio::test]
async fn test_send_does_not_share_a_token_across_providers() {
    let server = MockServer::start().await;
    let base = format!("{}/", server.uri());
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(Unauthenticated)
        .respond_with(challenge(&base))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"token":"tok"}"#))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(match_header("authorization", "Bearer tok"))
        .respond_with(ResponseTemplate::new(200))
        .expect(2)
        .mount(&server)
        .await;

    let upstream = Upstream::new();
    upstream
        .manifest(
            &upstream_client(&base, credentials(basic("alice", "pw"))),
            "library/nginx",
            "latest",
            &TokenRealms::default(),
        )
        .await
        .unwrap();
    upstream
        .manifest(
            &upstream_client(&base, credentials(basic("alice", "pw"))),
            "library/nginx",
            "latest",
            &TokenRealms::default(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn test_token_realm_401_refreshes_the_source_credential_once() {
    let server = MockServer::start().await;
    let base = format!("{}/", server.uri());
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(Unauthenticated)
        .respond_with(challenge(&base))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(match_header("authorization", basic_header("alice", "old").as_str()))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(match_header("authorization", basic_header("alice", "new").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"token":"tok"}"#))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(match_header("authorization", "Bearer tok"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let credentials = CredentialProvider::refreshing(
        basic("alice", "old"),
        CredentialRefresh {
            interval: Duration::from_mins(1),
            on_unauthorized: true,
            failure: CredentialFailure::Fail,
        },
        || async { Ok(basic("alice", "new")) },
    );
    let client = upstream_client(&base, credentials);

    let response = Upstream::new()
        .manifest(&client, "library/nginx", "latest", &TokenRealms::default())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_manifest_reports_a_scheduled_credential_refresh_failure() {
    let server = MockServer::start().await;
    let base = format!("{}/", server.uri());
    let credentials = CredentialProvider::refreshing(
        Auth::None,
        CredentialRefresh {
            interval: Duration::ZERO,
            on_unauthorized: true,
            failure: CredentialFailure::Fail,
        },
        || async { Err(CredentialError::new("source unavailable")) },
    );
    let client = upstream_client(&base, credentials);

    let result = Upstream::new()
        .manifest(&client, "library/nginx", "latest", &TokenRealms::default())
        .await;

    assert!(matches!(result, Err(UpstreamError::Transport(message)) if message == "source unavailable"));
}

#[tokio::test]
async fn test_token_realm_reports_a_source_refresh_failure() {
    let server = MockServer::start().await;
    let base = format!("{}/", server.uri());
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(Unauthenticated)
        .respond_with(challenge(&base))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    let credentials = CredentialProvider::refreshing(
        Auth::None,
        CredentialRefresh {
            interval: Duration::from_mins(1),
            on_unauthorized: true,
            failure: CredentialFailure::Fail,
        },
        || async { Err(CredentialError::new("source unavailable")) },
    );
    let client = upstream_client(&base, credentials);

    let result = Upstream::new()
        .manifest(&client, "library/nginx", "latest", &TokenRealms::default())
        .await;

    assert!(matches!(result, Err(UpstreamError::Transport(message)) if message == "source unavailable"));
}

#[tokio::test]
async fn test_token_realm_401_stops_when_refresh_is_disabled() {
    let server = MockServer::start().await;
    let base = format!("{}/", server.uri());
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(Unauthenticated)
        .respond_with(challenge(&base))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;

    let result = Upstream::new()
        .manifest(
            &upstream_client(&base, credentials(Auth::None)),
            "library/nginx",
            "latest",
            &TokenRealms::default(),
        )
        .await;

    assert!(matches!(result, Err(UpstreamError::Status(StatusCode::UNAUTHORIZED))));
}

#[tokio::test]
async fn test_send_reuses_a_cached_token_for_the_same_credentials() {
    let server = MockServer::start().await;
    let base = format!("{}/", server.uri());
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(Unauthenticated)
        .respond_with(challenge(&base))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"token":"tok"}"#))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(match_header("authorization", "Bearer tok"))
        .respond_with(ResponseTemplate::new(200))
        .expect(2)
        .mount(&server)
        .await;

    let upstream = Upstream::new();
    let client = upstream_client(&base, credentials(basic("alice", "pw1")));
    upstream
        .manifest(&client, "library/nginx", "latest", &TokenRealms::default())
        .await
        .unwrap();
    upstream
        .manifest(&client, "library/nginx", "latest", &TokenRealms::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn test_send_discards_a_cached_token_after_credential_refresh() {
    let server = MockServer::start().await;
    let base = format!("{}/", server.uri());
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(Unauthenticated)
        .respond_with(challenge(&base))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"token":"tok"}"#))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(match_header("authorization", "Bearer tok"))
        .respond_with(ResponseTemplate::new(200))
        .expect(2)
        .mount(&server)
        .await;
    let credentials = CredentialProvider::refreshing(
        basic("alice", "pw"),
        CredentialRefresh {
            interval: Duration::ZERO,
            on_unauthorized: false,
            failure: CredentialFailure::Fail,
        },
        || async { Ok(basic("alice", "pw")) },
    );
    let upstream = Upstream::new();
    let client = upstream_client(&base, credentials);

    upstream
        .manifest(&client, "library/nginx", "latest", &TokenRealms::default())
        .await
        .unwrap();
    upstream
        .manifest(&client, "library/nginx", "latest", &TokenRealms::default())
        .await
        .unwrap();
}

/// The barrier makes all pulls contend for one cold token.
async fn concurrent_pulls(
    upstream: &Arc<Upstream>,
    client: &UpstreamClient,
    count: usize,
) -> Vec<Result<StatusCode, UpstreamError>> {
    let barrier = Arc::new(Barrier::new(count));
    let mut pulls = Vec::with_capacity(count);
    for _ in 0..count {
        let upstream = Arc::clone(upstream);
        let client = client.clone();
        let barrier = Arc::clone(&barrier);
        pulls.push(tokio::spawn(async move {
            barrier.wait().await;
            upstream
                .manifest(&client, "library/nginx", "latest", &TokenRealms::default())
                .await
                .map(|response| response.status())
        }));
    }
    let mut outcomes = Vec::with_capacity(count);
    for pull in pulls {
        outcomes.push(pull.await.unwrap());
    }
    outcomes
}

#[tokio::test]
async fn test_token_flight_wait_has_a_deadline() {
    let (_sender, receiver) = broadcast::channel(1);

    assert_eq!(
        wait_for_flight(receiver, Instant::now()).await.unwrap_err().to_string(),
        "token exchange wait timed out"
    );
}

#[tokio::test]
async fn test_token_flight_retries_when_the_sender_closes() {
    let upstream = Upstream::new();
    let credentials = credentials(basic("alice", "pw"));
    let credential = credentials.credential().await.unwrap();
    let client = upstream_client("https://registry.example/", credentials.clone());
    let cache_key = token_cache_key(
        "https://registry.example/",
        "repository:library/nginx:pull",
        credential.identity().provider(),
    );
    let (sender, _) = broadcast::channel(1);
    upstream.inflight.lock().await.insert(cache_key.clone(), sender.clone());
    let close = async {
        tokio::task::yield_now().await;
        assert_eq!(sender.receiver_count(), 1);
        upstream.tokens.lock().await.insert(
            cache_key.clone(),
            CachedToken {
                credentials: credential.identity(),
                value: "retried".to_owned(),
            },
        );
        upstream.inflight.lock().await.remove(&cache_key);
        drop(sender);
    };
    let challenge = Bearer {
        realm: "unreachable".to_owned(),
        service: None,
        scope: None,
    };

    let realms = TokenRealms::default();
    let exchange = TokenExchange {
        challenge: &challenge,
        credentials: &credentials,
        credential: &credential,
        realms: &realms,
    };
    let ((), token) = tokio::join!(
        biased;
        close,
        upstream.acquire_token(&client, &cache_key, None, &exchange),
    );

    assert_eq!(token.unwrap(), "retried");
}

#[tokio::test]
async fn test_token_flight_waiter_returns_the_leader_token() {
    let upstream = Upstream::new();
    let credentials = credentials(basic("alice", "pw"));
    let credential = credentials.credential().await.unwrap();
    let client = upstream_client("https://registry.example/", credentials.clone());
    let cache_key = token_cache_key(
        "https://registry.example/",
        "repository:library/nginx:pull",
        credential.identity().provider(),
    );
    let (sender, _) = broadcast::channel(1);
    upstream.inflight.lock().await.insert(cache_key.clone(), sender.clone());
    let challenge = Bearer {
        realm: "unreachable".to_owned(),
        service: None,
        scope: None,
    };

    let realms = TokenRealms::default();
    let exchange = TokenExchange {
        challenge: &challenge,
        credentials: &credentials,
        credential: &credential,
        realms: &realms,
    };
    let (token, _) = tokio::join!(
        biased;
        upstream.acquire_token(&client, &cache_key, None, &exchange),
        async { sender.send("shared".to_owned()).unwrap() },
    );

    assert_eq!(token.unwrap(), "shared");
}

#[tokio::test]
async fn test_token_flight_reuses_a_token_cached_after_the_registry_request() {
    let upstream = Upstream::new();
    let credentials = credentials(basic("alice", "pw"));
    let credential = credentials.credential().await.unwrap();
    let base = "https://registry.example/";
    let client = upstream_client(base, credentials.clone());
    let scope = "repository:library/nginx:pull";
    let cache_key = token_cache_key(base, scope, credential.identity().provider());
    upstream.tokens.lock().await.insert(
        cache_key.clone(),
        CachedToken {
            credentials: credential.identity(),
            value: "cached".to_owned(),
        },
    );
    let challenge = Bearer {
        realm: "unreachable".to_owned(),
        service: None,
        scope: None,
    };
    let realms = TokenRealms::default();
    let exchange = TokenExchange {
        challenge: &challenge,
        credentials: &credentials,
        credential: &credential,
        realms: &realms,
    };

    assert_eq!(
        upstream
            .acquire_token(&client, &cache_key, None, &exchange)
            .await
            .unwrap(),
        "cached"
    );
}

#[tokio::test]
async fn test_token_exchange_has_a_deadline() {
    let server = MockServer::start().await;
    let base = format!("{}/", server.uri());
    let token_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let token_base = format!("http://{}/", token_listener.local_addr().unwrap());
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(Unauthenticated)
        .respond_with(challenge(&token_base))
        .mount(&server)
        .await;
    let mut upstream = Upstream::new();
    upstream.token_flight_timeout = Duration::from_millis(100);
    let client = upstream_client(&base, credentials(basic("alice", "pw")));
    let manifest = tokio::spawn(async move {
        upstream
            .manifest(&client, "library/nginx", "latest", &TokenRealms::default())
            .await
            .unwrap_err()
            .to_string()
    });
    let _connection = token_listener.accept().await.unwrap();

    assert_eq!(manifest.await.unwrap(), "token exchange timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_send_coalesces_concurrent_token_exchanges() {
    let server = MockServer::start().await;
    let base = format!("{}/", server.uri());
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(Unauthenticated)
        .respond_with(challenge(&base))
        .mount(&server)
        .await;
    let (gate, response) = gated_response(ResponseTemplate::new(200).set_body_string(r#"{"token":"tok"}"#));
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(response)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(match_header("authorization", "Bearer tok"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let upstream = Arc::new(Upstream::new());
    let client = upstream_client(&base, credentials(basic("alice", "pw")));
    let pulls = tokio::spawn({
        let upstream = Arc::clone(&upstream);
        let client = client.clone();
        async move { concurrent_pulls(&upstream, &client, 8).await }
    });
    drop(gate.entered().await);
    let outcomes = pulls.await.unwrap();

    for outcome in outcomes {
        assert_eq!(outcome.unwrap(), StatusCode::OK);
    }
    let cached = {
        let tokens = upstream.tokens.lock().await;
        tokens.values().map(|token| token.value.clone()).collect::<Vec<_>>()
    };
    assert_eq!(cached, ["tok"]);
}

/// The fixture verifies waiter re-election after leader failure.
struct FailThenIssueToken {
    calls: AtomicUsize,
    first: ResponseGate,
}
impl wiremock::Respond for FailThenIssueToken {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.first.block();
            ResponseTemplate::new(500)
        } else {
            ResponseTemplate::new(200).set_body_string(r#"{"token":"tok"}"#)
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_send_reelects_a_leader_after_a_failed_exchange() {
    let server = MockServer::start().await;
    let base = format!("{}/", server.uri());
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(Unauthenticated)
        .respond_with(challenge(&base))
        .mount(&server)
        .await;
    let first = response_gate();
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(FailThenIssueToken {
            calls: AtomicUsize::new(0),
            first: first.clone(),
        })
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(match_header("authorization", "Bearer tok"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let upstream = Arc::new(Upstream::new());
    let client = upstream_client(&base, credentials(basic("alice", "pw")));
    let pulls = tokio::spawn({
        let upstream = Arc::clone(&upstream);
        let client = client.clone();
        async move { concurrent_pulls(&upstream, &client, 2).await }
    });
    drop(first.entered().await);
    let outcomes = pulls.await.unwrap();

    assert_eq!(
        (
            outcomes.len(),
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, Ok(StatusCode::OK)))
                .count(),
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, Err(UpstreamError::Status(StatusCode::INTERNAL_SERVER_ERROR))))
                .count(),
        ),
        (2, 1, 1)
    );
}
