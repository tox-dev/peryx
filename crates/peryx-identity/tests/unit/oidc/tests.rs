use std::collections::BTreeMap;
use std::sync::Arc;

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use rstest::rstest;
use serde::Serialize;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

const NOW: i64 = 2_000_000_000;
const MODULUS: &str = "yRE6rHuNR0QbHO3H3Kt2pOKGVhQqGZXInOduQNxXzuKlvQTLUTv4l4sggh5_CYYi_cvI-SXVT9kPWSKXxJXBXd_4LkvcPuUakBoAkfh-eiFVMh2VrUyWyj3MFl0HTVF9KwRXLAcwkREiS3npThHRyIxuy0ZMeZfxVL5arMhw1SRELB8HoGfG_AtH89BIE9jDBHZ9dLelK9a184zAf8LwoPLxvJb3Il5nncqPcSfKDDodMFBIMc4lQzDKL5gvmiXLXB1AGLm8KBjfE8s3L5xqi-yUod-j8MtvIj812dkS4QMiRVN_by2h3ZY8LYVGrqZXZTcgn2ujn8uKjXLZVD5TdQ";
const PRIVATE_KEY_DER: &str = "MIIEpAIBAAKCAQEAyRE6rHuNR0QbHO3H3Kt2pOKGVhQqGZXInOduQNxXzuKlvQTLUTv4l4sggh5/CYYi/cvI+SXVT9kPWSKXxJXBXd/4LkvcPuUakBoAkfh+eiFVMh2VrUyWyj3MFl0HTVF9KwRXLAcwkREiS3npThHRyIxuy0ZMeZfxVL5arMhw1SRELB8HoGfG/AtH89BIE9jDBHZ9dLelK9a184zAf8LwoPLxvJb3Il5nncqPcSfKDDodMFBIMc4lQzDKL5gvmiXLXB1AGLm8KBjfE8s3L5xqi+yUod+j8MtvIj812dkS4QMiRVN/by2h3ZY8LYVGrqZXZTcgn2ujn8uKjXLZVD5TdQIDAQABAoIBAHREk0I0O9DvECKdWUpAmF3mY7oY9PNQiu44Yaf+AoSuyRpRUGTMIgc3u3eivOE8ALX0BmYUO5JtuRNZDpvt4SAwqCnVUinIf6C+eH/wSurCpapSM0BAHp4aOA7igptyOMgMPYBHNA1e9A7jE0dCxKWMl3DSWNyjQTk4zeRGEAEfbNjHrq6YCtjHSZSLmWiG80hnfnYos9hOr5JnLnyS7ZmFE/5P3XVrxLc/tQ5zum0R4cbrgzHiQP5RgfxGJaEi7XcgherCCOgurJSSbYH29Gz8u5fFbS+Yg8s+OiCss3cs1rSgJ9/eHZuzGEdUZVARH6hVMjSuwvqVTFaE8AgtleECgYEA+uLMn4kNqHlJS2A5uAnCkj90ZxEtNm3E8hAxUrhssktY5XSOAPBlxyf5RuRGIImGtUVIr4HuJSa5TX48n3Vdt9MYCprO/iYl6moNRSPt5qowIIOJmIjY2mqPDfDt/zw+fcDD3lmCJrFlzcnh0uea1CohxEbQnL3cypeLt+WbU6kCgYEAzSp19m1ajieFkqgoB0YTpt/OroDx38vvI5unInJlEeOjQ+oIAQdN2wpxBvTrRorMU6P07mFUbt1j+Co6CbNiw+X8HcCaqYLR5clbJOOWNR36PuzOpQLkfK8woupBxzW9B8gZmY8rB1mbJ+/WTPrEJy6YGmIEBkWylQ2VpW8O4O0CgYEApdbvvfFBlwD9YxbrcGz7MeNCFbMz+MucqQntIKoKJ91ImPxvtc0y6e/Rhnv0oyNlaUOwJVu0yNgNG117w0g4t/+Q38mvVC5xV7/cn7x9UMFk6MkqVir3dYGEqIl/OP1grY2Tq9HtB5iyG9L8NIamQOLMyUqqMUILxdthHyFmiGkCgYEAn9+PjpjGMPHxL0gj8Q8VbzsFtou6b1deIRRA2CHmSltltR1gYVTMwXxQeUhPMmgkMqUXzs4/WijgpthY44hK1TaZEKIuoxrS70nJ4WQLf5a9k1065fDsFZD6yGjdGxvwEmlGMZgTwqV7t1I4X0Ilqhav5hcs5apYL7gnPYPeRz0CgYALHCj/Ji8XSsDoF/MhVhnGdIs2P99NNdmo3R2Pv0CuZbDKMU559LJHUvrKS8WkuWRDuKrz1W/EQKApFjDGpdqToZqriUFQzwy7mR3ayIiogzNtHcvbDHx8oFnGY0OFksX/ye0/XGpy2SFxYRwGU98HPYeBvAQQrVjdkzfy7BmXQQ==";

#[derive(Serialize)]
struct Claims<'a> {
    iss: &'a str,
    aud: Value,
    sub: &'a str,
    exp: i64,
    iat: i64,
    nbf: i64,
    jti: &'a str,
    repository_id: &'a str,
}

fn binding(issuer: &str) -> PublisherBinding {
    PublisherBinding {
        id: "github-release".to_owned(),
        repository: "private".to_owned(),
        publisher: TrustedPublisher {
            issuer: issuer.to_owned(),
            audience: "peryx".to_owned(),
            subject: Glob::new("repo:org/app:*"),
            claims: BTreeMap::from([("repository_id".to_owned(), "42".to_owned())]),
            projects: vec![Glob::new("app")],
        },
    }
}

fn test_runtime(issuer: &str) -> OidcRuntime {
    OidcRuntime::build(
        vec![binding(issuer)],
        Signer::new(b"local-key", "peryx"),
        300,
        true,
        MAX_REPLAY_ENTRIES,
    )
    .unwrap()
}

fn test_runtime_with_replay_capacity(issuer: &str, capacity: usize) -> OidcRuntime {
    OidcRuntime::build(
        vec![binding(issuer)],
        Signer::new(b"local-key", "peryx"),
        300,
        true,
        capacity,
    )
    .unwrap()
}

async fn mount_issuer_with(server: &MockServer, keys: Value, content_type: &str, cache_control: Option<&str>) {
    let mut discovery = ResponseTemplate::new(200).set_body_raw(
        json!({
            "issuer": server.uri(),
            "jwks_uri": format!("{}/keys", server.uri()),
            "id_token_signing_alg_values_supported": ["RS256"]
        })
        .to_string(),
        content_type,
    );
    let mut jwks = ResponseTemplate::new(200)
        .insert_header("content-type", "application/json")
        .set_body_json(keys);
    if let Some(cache_control) = cache_control {
        discovery = discovery.insert_header("cache-control", cache_control);
        jwks = jwks.insert_header("cache-control", cache_control);
    }
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(discovery)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/keys"))
        .respond_with(jwks)
        .mount(server)
        .await;
}

async fn mount_issuer(server: &MockServer, keys: Value) {
    mount_issuer_with(server, keys, "application/json", Some("max-age=120")).await;
}

fn jwk(kid: &str) -> Value {
    json!({"kty": "RSA", "n": MODULUS, "e": "AQAB", "kid": kid, "alg": "RS256", "use": "sig"})
}

fn encoding_key() -> EncodingKey {
    use base64::Engine as _;
    EncodingKey::from_rsa_der(
        &base64::engine::general_purpose::STANDARD
            .decode(PRIVATE_KEY_DER)
            .unwrap(),
    )
}

fn identity_with_expiry(issuer: &str, kid: &str, jti: &str, expires_at: i64) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(kid.to_owned());
    jsonwebtoken::encode(
        &header,
        &Claims {
            iss: issuer,
            aud: Value::String("peryx".to_owned()),
            sub: "repo:org/app:ref:refs/heads/main",
            exp: expires_at,
            iat: NOW,
            nbf: NOW,
            jti,
            repository_id: "42",
        },
        &encoding_key(),
    )
    .unwrap()
}

fn identity(issuer: &str, kid: &str, jti: &str) -> String {
    identity_with_expiry(issuer, kid, jti, NOW + 600)
}

async fn runtime() -> (MockServer, Arc<OidcRuntime>) {
    let server = MockServer::start().await;
    mount_issuer(&server, json!({"keys": [jwk("key-1")]})).await;
    let runtime = Arc::new(test_runtime(&server.uri()));
    (server, runtime)
}

#[tokio::test]
async fn test_exchange_mints_a_route_scoped_token_once() {
    let (server, runtime) = runtime().await;
    let external = identity(&server.uri(), "key-1", "external-1");
    let exchanged = runtime.exchange(&external, NOW).await.unwrap();
    let internal = Signer::new(b"local-key", "peryx")
        .verify_trusted(&exchanged.token)
        .unwrap();
    assert_eq!(exchanged.publisher_id, "github-release");
    assert_eq!(internal.id, exchanged.token_id);
    assert!(crate::authorize_grants(&internal.grants, Some("private/app"), crate::Action::Write).is_ok());
    assert!(crate::authorize_grants(&internal.grants, Some("other/app"), crate::Action::Write).is_err());
    assert!(matches!(
        runtime.exchange(&external, NOW).await,
        Err(ExchangeError::Replay)
    ));
}

#[tokio::test]
async fn test_identity_exchange_dispatches_through_the_contract() {
    let (server, runtime) = runtime().await;
    let exchange: &dyn IdentityExchange = runtime.as_ref();

    assert_eq!(exchange.audience(), "peryx");
    assert_eq!(
        exchange
            .exchange(&identity(&server.uri(), "key-1", "contract"), NOW)
            .await
            .unwrap()
            .publisher_id,
        "github-release"
    );
}

#[tokio::test]
async fn test_concurrent_exchange_has_one_winner() {
    let (server, runtime) = runtime().await;
    let token = identity(&server.uri(), "key-1", "race");
    let (first, second) = tokio::join!(runtime.exchange(&token, NOW), runtime.exchange(&token, NOW));
    assert_eq!(
        (
            usize::from(first.is_ok()) + usize::from(second.is_ok()),
            usize::from(matches!(first, Err(ExchangeError::Replay)))
                + usize::from(matches!(second, Err(ExchangeError::Replay))),
        ),
        (1, 1)
    );
}

#[tokio::test]
async fn test_duplicate_key_ids_reject_the_refresh() {
    let server = MockServer::start().await;
    mount_issuer(&server, json!({"keys": [jwk("same"), jwk("same")]})).await;
    let runtime = test_runtime(&server.uri());
    assert!(matches!(
        runtime.exchange(&identity(&server.uri(), "same", "jti"), NOW).await,
        Err(ExchangeError::InvalidIssuerResponse)
    ));
}

#[tokio::test]
async fn test_missing_key_id_rejects_the_refresh() {
    let server = MockServer::start().await;
    mount_issuer(
        &server,
        json!({"keys": [{"kty": "RSA", "n": MODULUS, "e": "AQAB", "alg": "RS256"}]}),
    )
    .await;
    let runtime = test_runtime(&server.uri());
    assert!(matches!(
        runtime.exchange(&identity(&server.uri(), "key-1", "jti"), NOW).await,
        Err(ExchangeError::InvalidIssuerResponse)
    ));
}

#[tokio::test]
async fn test_incompatible_keys_do_not_hide_a_usable_key() {
    let server = MockServer::start().await;
    mount_issuer(
        &server,
        json!({"keys": [
            {"kty": "oct", "k": "c2VjcmV0", "kid": "symmetric", "alg": "HS256"},
            {"kty": "RSA", "n": MODULUS, "e": "AQAB", "kid": "signing-only", "alg": "RS256", "use": "sig", "key_ops": ["sign"]},
            {"kty": "RSA", "n": MODULUS, "e": "AQAB", "kid": "key-1", "alg": "RS256", "use": "sig", "key_ops": ["verify"]}
        ]}),
    )
    .await;
    let runtime = test_runtime(&server.uri());
    assert!(
        runtime
            .exchange(&identity(&server.uri(), "key-1", "mixed"), NOW)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn test_key_set_without_a_usable_key_is_rejected() {
    let server = MockServer::start().await;
    mount_issuer(
        &server,
        json!({"keys": [
            {"kty": "oct", "k": "c2VjcmV0", "kid": "symmetric", "alg": "HS256"}
        ]}),
    )
    .await;
    let runtime = test_runtime(&server.uri());
    assert!(matches!(
        runtime.exchange(&identity(&server.uri(), "key-1", "jti"), NOW).await,
        Err(ExchangeError::InvalidIssuerResponse)
    ));
}

#[tokio::test]
async fn test_malformed_refresh_keeps_the_working_key() {
    let (server, runtime) = runtime().await;
    runtime
        .exchange(&identity(&server.uri(), "key-1", "warm"), NOW)
        .await
        .unwrap();
    server.reset().await;
    mount_issuer(
        &server,
        json!({"keys": [{"kty": "RSA", "n": "!", "e": "AQAB", "kid": "key-1", "alg": "RS256"}]}),
    )
    .await;
    assert!(
        runtime
            .exchange(&identity(&server.uri(), "key-1", "stale"), NOW + 121)
            .await
            .is_ok()
    );
    assert!(
        runtime
            .exchange(&identity(&server.uri(), "key-1", "cached"), NOW + 122)
            .await
            .is_ok()
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    assert!(matches!(
        runtime
            .exchange(&identity(&server.uri(), "key-1", "expired"), NOW + HARD_CACHE_SECS + 1)
            .await,
        Err(ExchangeError::InvalidIssuerResponse)
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 4);
}

#[tokio::test]
async fn test_replay_capacity_recovers_after_identity_expiry() {
    let (server, _runtime) = runtime().await;
    let runtime = test_runtime_with_replay_capacity(&server.uri(), 1);
    runtime
        .exchange(&identity_with_expiry(&server.uri(), "key-1", "first", NOW + 1), NOW)
        .await
        .unwrap();
    assert!(matches!(
        runtime.exchange(&identity(&server.uri(), "key-1", "full"), NOW).await,
        Err(ExchangeError::ReplayCapacity)
    ));
    assert!(
        runtime
            .exchange(&identity(&server.uri(), "key-1", "recovered"), NOW + 2)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn test_bad_signature_does_not_refresh_a_warm_key() {
    let (server, runtime) = runtime().await;
    runtime
        .exchange(&identity(&server.uri(), "key-1", "warm"), NOW)
        .await
        .unwrap();
    let mut bad = identity(&server.uri(), "key-1", "bad");
    bad.push('x');
    assert!(matches!(
        runtime.exchange(&bad, NOW).await,
        Err(ExchangeError::InvalidIdentity)
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn test_cold_issuer_failure_is_rate_limited() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let runtime = test_runtime(&server.uri());
    let token = identity(&server.uri(), "key-1", "cold");
    assert!(matches!(runtime.exchange(&token, NOW).await, Err(error) if error.unavailable()));
    assert!(matches!(runtime.exchange(&token, NOW).await, Err(error) if error.unavailable()));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_unknown_key_refresh_is_single_flight() {
    let (server, runtime) = runtime().await;
    runtime
        .exchange(&identity(&server.uri(), "key-1", "warm"), NOW)
        .await
        .unwrap();
    let unknown = identity(&server.uri(), "key-2", "unknown");
    let (first, second) = tokio::join!(
        runtime.exchange(&unknown, NOW + 61),
        runtime.exchange(&unknown, NOW + 61)
    );
    assert!(matches!(first, Err(ExchangeError::UnknownKey)));
    assert!(matches!(second, Err(ExchangeError::UnknownKey)));
    assert_eq!(server.received_requests().await.unwrap().len(), 4);
}

#[tokio::test]
async fn test_claim_time_and_shape_failures_are_closed() {
    let (server, runtime) = runtime().await;
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("key-1".to_owned());
    let key = encoding_key();
    for (jti, claims) in [
        (
            "multi-aud",
            Claims {
                iss: &server.uri(),
                aud: json!(["peryx", "other"]),
                sub: "repo:org/app:x",
                exp: NOW + 10,
                iat: NOW,
                nbf: NOW,
                jti: "multi-aud",
                repository_id: "42",
            },
        ),
        (
            "future",
            Claims {
                iss: &server.uri(),
                aud: json!("peryx"),
                sub: "repo:org/app:x",
                exp: NOW + 20,
                iat: NOW + 10,
                nbf: NOW + 10,
                jti: "future",
                repository_id: "42",
            },
        ),
        (
            "long",
            Claims {
                iss: &server.uri(),
                aud: json!("peryx"),
                sub: "repo:org/app:x",
                exp: NOW + MAX_IDENTITY_LIFETIME_SECS + 1,
                iat: NOW,
                nbf: NOW,
                jti: "long",
                repository_id: "42",
            },
        ),
        (
            "overflow",
            Claims {
                iss: &server.uri(),
                aud: json!("peryx"),
                sub: "repo:org/app:x",
                exp: i64::MAX,
                iat: i64::MIN,
                nbf: i64::MIN,
                jti: "overflow",
                repository_id: "42",
            },
        ),
    ] {
        let token = jsonwebtoken::encode(&header, &claims, &key).unwrap();
        assert!(
            matches!(runtime.exchange(&token, NOW).await, Err(ExchangeError::InvalidIdentity)),
            "{jti}"
        );
    }
}

#[tokio::test]
async fn test_claim_text_bounds_are_closed() {
    let (server, runtime) = runtime().await;
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("key-1".to_owned());
    for (name, sub, jti) in [
        ("subject", "s".repeat(MAX_SUBJECT_BYTES + 1), "jti".to_owned()),
        ("jti", "subject".to_owned(), "j".repeat(MAX_JTI_BYTES + 1)),
    ] {
        let token = jsonwebtoken::encode(
            &header,
            &Claims {
                iss: &server.uri(),
                aud: json!("peryx"),
                sub: &sub,
                exp: NOW + 60,
                iat: NOW,
                nbf: NOW,
                jti: &jti,
                repository_id: "42",
            },
            &encoding_key(),
        )
        .unwrap();
        assert!(
            matches!(runtime.exchange(&token, NOW).await, Err(ExchangeError::InvalidIdentity)),
            "{name}"
        );
    }
}

#[tokio::test]
async fn test_token_size_and_algorithm_are_closed() {
    let (server, runtime) = runtime().await;
    assert!(matches!(
        runtime.exchange(&"x".repeat(TOKEN_BODY_LIMIT + 1), NOW).await,
        Err(ExchangeError::InvalidIdentity)
    ));
    let token = jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &Claims {
            iss: &server.uri(),
            aud: json!("peryx"),
            sub: "repo:org/app:x",
            exp: NOW + 60,
            iat: NOW,
            nbf: NOW,
            jti: "wrong-algorithm",
            repository_id: "42",
        },
        &EncodingKey::from_secret(b"secret"),
    )
    .unwrap();
    assert!(matches!(
        runtime.exchange(&token, NOW).await,
        Err(ExchangeError::InvalidIdentity)
    ));
}

#[tokio::test]
async fn test_discovery_must_repeat_the_issuer_and_algorithm() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "issuer": "https://other.example",
                    "jwks_uri": format!("{}/keys", server.uri()),
                    "id_token_signing_alg_values_supported": ["ES256"]
                })),
        )
        .mount(&server)
        .await;
    let runtime = test_runtime(&server.uri());
    assert!(matches!(
        runtime.exchange(&identity(&server.uri(), "key-1", "jti"), NOW).await,
        Err(ExchangeError::InvalidIssuerResponse)
    ));
}

#[tokio::test]
async fn test_chunked_issuer_body_is_bounded_while_streaming() {
    use std::io::{Read as _, Write as _};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut request = [0; 1024];
        let _ = socket.read(&mut request);
        let body = "x".repeat(DISCOVERY_BODY_LIMIT + 1);
        write!(
            socket,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n{:X}\r\n{body}\r\n0\r\n\r\n",
            body.len()
        )
        .unwrap();
    });
    let issuer = format!("http://{address}");
    let runtime = test_runtime(&issuer);
    assert!(matches!(
        runtime.exchange(&identity(&issuer, "key-1", "large"), NOW).await,
        Err(ExchangeError::InvalidIssuerResponse)
    ));
}

#[tokio::test]
async fn test_truncated_issuer_body_is_unavailable() {
    use std::io::{Read as _, Write as _};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut request = [0; 1024];
        let _ = socket.read(&mut request);
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 8\r\nconnection: close\r\n\r\n{}",
            )
            .unwrap();
    });
    let issuer = format!("http://{address}");
    let runtime = test_runtime(&issuer);
    assert!(matches!(
        runtime.exchange(&identity(&issuer, "key-1", "truncated"), NOW).await,
        Err(ExchangeError::IssuerUnavailable)
    ));
}

#[test]
fn test_runtime_rejects_an_empty_publisher_set() {
    assert!(matches!(
        OidcRuntime::new(Vec::new(), Signer::new(b"key", "peryx"), 60),
        Err(ExchangeError::Configuration)
    ));
}

#[test]
fn test_runtime_rejects_a_nonpositive_token_lifetime() {
    assert!(matches!(
        OidcRuntime::new(vec![binding("https://issuer.example")], Signer::new(b"key", "peryx"), 0,),
        Err(ExchangeError::Configuration)
    ));
}

#[test]
fn test_runtime_rejects_an_empty_publisher_id() {
    let mut invalid_id = binding("https://issuer.example");
    invalid_id.id.clear();
    assert!(matches!(
        OidcRuntime::new(vec![invalid_id], Signer::new(b"key", "peryx"), 60),
        Err(ExchangeError::Configuration)
    ));
}

#[rstest]
#[case::malformed("not a URL")]
#[case::http("http://id.example")]
#[case::credentials("https://user@id.example")]
fn test_runtime_rejects_an_invalid_issuer(#[case] issuer: &str) {
    assert!(matches!(
        OidcRuntime::new(vec![binding(issuer)], Signer::new(b"key", "peryx"), 60),
        Err(ExchangeError::Configuration)
    ));
}

#[test]
fn test_production_runtime_accepts_an_https_issuer() {
    let runtime = OidcRuntime::new(
        vec![binding("https://issuer.example")],
        Signer::new(b"key", "peryx"),
        60,
    )
    .unwrap();
    assert_eq!(runtime.audience(), "peryx");
}

#[tokio::test]
async fn test_discovery_uses_the_configured_issuer_path() {
    let server = MockServer::start().await;
    let issuer = format!("{}/tenant/", server.uri());
    Mock::given(method("GET"))
        .and(path("/tenant/.well-known/openid-configuration"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "issuer": &issuer,
                    "jwks_uri": format!("{}/keys", server.uri()),
                    "id_token_signing_alg_values_supported": ["RS256"]
                })),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/keys"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({"keys": [jwk("key-1")]})),
        )
        .mount(&server)
        .await;
    let runtime = test_runtime(&issuer);
    assert!(runtime.exchange(&identity(&issuer, "key-1", "path"), NOW).await.is_ok());
}

#[rstest]
#[case::quoted_zero(Some("private, max-age=\"0\", must-revalidate"), 60)]
#[case::absent(None, 300)]
#[tokio::test]
async fn test_cache_control_sets_refresh_time(#[case] cache_control: Option<&str>, #[case] fresh_for: i64) {
    let server = MockServer::start().await;
    mount_issuer_with(
        &server,
        json!({"keys": [jwk("key-1")]}),
        "application/json",
        cache_control,
    )
    .await;
    let runtime = test_runtime(&server.uri());
    runtime
        .exchange(&identity(&server.uri(), "key-1", "cold"), NOW)
        .await
        .unwrap();
    runtime
        .exchange(&identity(&server.uri(), "key-1", "fresh"), NOW + fresh_for - 1)
        .await
        .unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    runtime
        .exchange(&identity(&server.uri(), "key-1", "refresh"), NOW + fresh_for)
        .await
        .unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 4);
}

#[rstest]
#[case::json("application/json; charset=utf-8", true)]
#[case::structured("application/jwk-set+json", true)]
#[case::case_insensitive("Application/JSON", true)]
#[case::wrong_type("text/json", false)]
#[tokio::test]
async fn test_discovery_content_type_is_enforced(#[case] content_type: &str, #[case] accepted: bool) {
    let server = MockServer::start().await;
    mount_issuer_with(
        &server,
        json!({"keys": [jwk("key-1")]}),
        content_type,
        Some("max-age=120"),
    )
    .await;
    let result = test_runtime(&server.uri())
        .exchange(&identity(&server.uri(), "key-1", "content-type"), NOW)
        .await;
    if accepted {
        assert!(result.is_ok());
    } else {
        assert!(matches!(result, Err(ExchangeError::InvalidIssuerResponse)));
    }
}

#[tokio::test]
async fn test_empty_repository_keeps_project_grants_unqualified() {
    let server = MockServer::start().await;
    mount_issuer(&server, json!({"keys": [jwk("key-1")]})).await;
    let mut binding = binding(&server.uri());
    binding.repository.clear();
    let signer = Signer::new(b"local-key", "peryx");
    let runtime = OidcRuntime::build(vec![binding], signer.clone(), 300, true, MAX_REPLAY_ENTRIES).unwrap();
    let exchanged = runtime
        .exchange(&identity(&server.uri(), "key-1", "unqualified"), NOW)
        .await
        .unwrap();
    assert_eq!(
        signer.verify_trusted(&exchanged.token).unwrap().grants,
        vec![Grant {
            projects: vec![Glob::new("app")],
            actions: std::collections::BTreeSet::from([crate::Action::Write]),
        }]
    );
}

#[rstest]
#[case::issuer_unavailable(ExchangeError::IssuerUnavailable, true)]
#[case::invalid_response(ExchangeError::InvalidIssuerResponse, true)]
#[case::unknown_key(ExchangeError::UnknownKey, true)]
#[case::replay_capacity(ExchangeError::ReplayCapacity, true)]
#[case::configuration(ExchangeError::Configuration, false)]
#[case::invalid_identity(ExchangeError::InvalidIdentity, false)]
#[case::replay(ExchangeError::Replay, false)]
#[case::denied(ExchangeError::Denied(PublishDenial::UnknownIssuer), false)]
fn test_exchange_error_availability(#[case] error: ExchangeError, #[case] expected: bool) {
    assert_eq!(error.unavailable(), expected);
}
