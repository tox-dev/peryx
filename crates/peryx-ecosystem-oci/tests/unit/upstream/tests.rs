use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use peryx_upstream::{CredentialFailure, CredentialProvider, CredentialRefresh};
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

use base64::Engine as _;
use wiremock::matchers::{header as match_header, method, path, query_param};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

/// Match only the token-less first attempt, so the bearer-carrying retry falls through to the 200.
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
        .manifest(base, &credentials(Auth::None), "library/nginx", "latest")
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
        .manifest(&base, &credentials(Auth::None), "library/nginx", "latest")
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
        .manifest(&base, &credentials(Auth::None), "library/nginx", "latest")
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
        .manifest(&base, &credentials(Auth::None), "library/nginx", "latest")
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
        .manifest(&base, &credentials(Auth::None), "library/nginx", "latest")
        .await;

    assert!(matches!(result, Err(UpstreamError::Transport(message)) if message.starts_with("invalid bearer realm:")));
}

#[tokio::test]
async fn test_fetch_token_refuses_basic_credentials_to_a_cleartext_realm() {
    let server = MockServer::start().await;
    let base = format!("{}/", server.uri());
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(401).insert_header(
            "www-authenticate",
            r#"Bearer realm="http://token.registry.example/token",service=reg"#,
        ))
        .expect(1)
        .mount(&server)
        .await;

    let result = Upstream::new()
        .manifest(&base, &credentials(basic("alice", "pw")), "library/nginx", "latest")
        .await;

    assert!(
        matches!(&result, Err(UpstreamError::Transport(message)) if message.starts_with("insecure bearer realm")),
        "expected a cleartext-realm refusal, got {result:?}"
    );
}

#[tokio::test]
async fn test_fetch_token_allows_basic_credentials_to_an_https_realm_on_another_host() {
    let server = MockServer::start().await;
    let base = format!("{}/", server.uri());
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(401).insert_header(
            "www-authenticate",
            r#"Bearer realm="https://auth.example.invalid/token",service=reg"#,
        ))
        .expect(1)
        .mount(&server)
        .await;

    let result = Upstream::new()
        .manifest(&base, &credentials(basic("alice", "pw")), "library/nginx", "latest")
        .await;

    // An https realm on a host other than the registry (Docker Hub's auth.docker.io works this way)
    // clears the scheme gate, so the token fetch is attempted and fails only on the unresolvable host.
    assert!(
        matches!(&result, Err(UpstreamError::Transport(message)) if !message.starts_with("insecure bearer realm")),
        "expected the realm accepted and the fetch attempted, got {result:?}"
    );
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
        .manifest(&base, &credentials(basic("alice", "pw")), "library/nginx", "latest")
        .await
        .unwrap();
    upstream
        .manifest(&base, &credentials(basic("alice", "pw")), "library/nginx", "latest")
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
        .and(match_header(
            "authorization",
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode("alice:old")
            )
            .as_str(),
        ))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(match_header(
            "authorization",
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode("alice:new")
            )
            .as_str(),
        ))
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

    let response = Upstream::new()
        .manifest(&base, &credentials, "library/nginx", "latest")
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

    let result = Upstream::new()
        .manifest(&base, &credentials, "library/nginx", "latest")
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

    let result = Upstream::new()
        .manifest(&base, &credentials, "library/nginx", "latest")
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
        .manifest(&base, &credentials(Auth::None), "library/nginx", "latest")
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
    let credentials = credentials(basic("alice", "pw1"));
    upstream
        .manifest(&base, &credentials, "library/nginx", "latest")
        .await
        .unwrap();
    upstream
        .manifest(&base, &credentials, "library/nginx", "latest")
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

    upstream
        .manifest(&base, &credentials, "library/nginx", "latest")
        .await
        .unwrap();
    upstream
        .manifest(&base, &credentials, "library/nginx", "latest")
        .await
        .unwrap();
}

/// Fire `count` pulls that release together at a barrier, so they contend for one cold token.
async fn concurrent_pulls(
    upstream: &Arc<Upstream>,
    base: &str,
    credentials: &CredentialProvider,
    count: usize,
) -> Vec<Result<StatusCode, UpstreamError>> {
    let barrier = Arc::new(Barrier::new(count));
    let mut pulls = Vec::with_capacity(count);
    for _ in 0..count {
        let upstream = Arc::clone(upstream);
        let credentials = credentials.clone();
        let base = base.to_owned();
        let barrier = Arc::clone(&barrier);
        pulls.push(tokio::spawn(async move {
            barrier.wait().await;
            upstream
                .manifest(&base, &credentials, "library/nginx", "latest")
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
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(200))
                .set_body_string(r#"{"token":"tok"}"#),
        )
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
    let credentials = credentials(basic("alice", "pw"));
    let outcomes = concurrent_pulls(&upstream, &base, &credentials, 8).await;

    for outcome in outcomes {
        assert_eq!(outcome.unwrap(), StatusCode::OK);
    }
    let cached = {
        let tokens = upstream.tokens.lock().await;
        tokens.values().map(|token| token.value.clone()).collect::<Vec<_>>()
    };
    assert_eq!(cached, ["tok"]);
}

/// The token endpoint fails the first exchange, then succeeds: the leader whose exchange fails
/// clears the in-flight slot, and a waiting pull re-elects one fresh leader that retries and wins.
struct FailThenIssueToken(AtomicUsize);
impl wiremock::Respond for FailThenIssueToken {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            ResponseTemplate::new(500).set_delay(Duration::from_millis(200))
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
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(FailThenIssueToken(AtomicUsize::new(0)))
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
    let credentials = credentials(basic("alice", "pw"));
    let outcomes = concurrent_pulls(&upstream, &base, &credentials, 2).await;

    let (mut succeeded, mut failed) = (0, 0);
    for outcome in outcomes {
        match outcome {
            Ok(status) => {
                assert_eq!(status, StatusCode::OK);
                succeeded += 1;
            }
            Err(UpstreamError::Status(StatusCode::INTERNAL_SERVER_ERROR)) => failed += 1,
            other => panic!("unexpected pull outcome: {other:?}"),
        }
    }
    assert_eq!((succeeded, failed), (1, 1));
}
