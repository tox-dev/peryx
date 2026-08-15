use std::sync::Arc;

use async_trait::async_trait;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use rstest::rstest;
use serde::Serialize;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::tests::oidc_http::{MalformedDiscoveryBody, MalformedDiscoveryServer, secure_origin, transport};

const NOW: i64 = 2_000_000_000;
const MODULUS: &str = "yRE6rHuNR0QbHO3H3Kt2pOKGVhQqGZXInOduQNxXzuKlvQTLUTv4l4sggh5_CYYi_cvI-SXVT9kPWSKXxJXBXd_4LkvcPuUakBoAkfh-eiFVMh2VrUyWyj3MFl0HTVF9KwRXLAcwkREiS3npThHRyIxuy0ZMeZfxVL5arMhw1SRELB8HoGfG_AtH89BIE9jDBHZ9dLelK9a184zAf8LwoPLxvJb3Il5nncqPcSfKDDodMFBIMc4lQzDKL5gvmiXLXB1AGLm8KBjfE8s3L5xqi-yUod-j8MtvIj812dkS4QMiRVN_by2h3ZY8LYVGrqZXZTcgn2ujn8uKjXLZVD5TdQ";
const PRIVATE_KEY_DER: &str = "MIIEpAIBAAKCAQEAyRE6rHuNR0QbHO3H3Kt2pOKGVhQqGZXInOduQNxXzuKlvQTLUTv4l4sggh5/CYYi/cvI+SXVT9kPWSKXxJXBXd/4LkvcPuUakBoAkfh+eiFVMh2VrUyWyj3MFl0HTVF9KwRXLAcwkREiS3npThHRyIxuy0ZMeZfxVL5arMhw1SRELB8HoGfG/AtH89BIE9jDBHZ9dLelK9a184zAf8LwoPLxvJb3Il5nncqPcSfKDDodMFBIMc4lQzDKL5gvmiXLXB1AGLm8KBjfE8s3L5xqi+yUod+j8MtvIj812dkS4QMiRVN/by2h3ZY8LYVGrqZXZTcgn2ujn8uKjXLZVD5TdQIDAQABAoIBAHREk0I0O9DvECKdWUpAmF3mY7oY9PNQiu44Yaf+AoSuyRpRUGTMIgc3u3eivOE8ALX0BmYUO5JtuRNZDpvt4SAwqCnVUinIf6C+eH/wSurCpapSM0BAHp4aOA7igptyOMgMPYBHNA1e9A7jE0dCxKWMl3DSWNyjQTk4zeRGEAEfbNjHrq6YCtjHSZSLmWiG80hnfnYos9hOr5JnLnyS7ZmFE/5P3XVrxLc/tQ5zum0R4cbrgzHiQP5RgfxGJaEi7XcgherCCOgurJSSbYH29Gz8u5fFbS+Yg8s+OiCss3cs1rSgJ9/eHZuzGEdUZVARH6hVMjSuwvqVTFaE8AgtleECgYEA+uLMn4kNqHlJS2A5uAnCkj90ZxEtNm3E8hAxUrhssktY5XSOAPBlxyf5RuRGIImGtUVIr4HuJSa5TX48n3Vdt9MYCprO/iYl6moNRSPt5qowIIOJmIjY2mqPDfDt/zw+fcDD3lmCJrFlzcnh0uea1CohxEbQnL3cypeLt+WbU6kCgYEAzSp19m1ajieFkqgoB0YTpt/OroDx38vvI5unInJlEeOjQ+oIAQdN2wpxBvTrRorMU6P07mFUbt1j+Co6CbNiw+X8HcCaqYLR5clbJOOWNR36PuzOpQLkfK8woupBxzW9B8gZmY8rB1mbJ+/WTPrEJy6YGmIEBkWylQ2VpW8O4O0CgYEApdbvvfFBlwD9YxbrcGz7MeNCFbMz+MucqQntIKoKJ91ImPxvtc0y6e/Rhnv0oyNlaUOwJVu0yNgNG117w0g4t/+Q38mvVC5xV7/cn7x9UMFk6MkqVir3dYGEqIl/OP1grY2Tq9HtB5iyG9L8NIamQOLMyUqqMUILxdthHyFmiGkCgYEAn9+PjpjGMPHxL0gj8Q8VbzsFtou6b1deIRRA2CHmSltltR1gYVTMwXxQeUhPMmgkMqUXzs4/WijgpthY44hK1TaZEKIuoxrS70nJ4WQLf5a9k1065fDsFZD6yGjdGxvwEmlGMZgTwqV7t1I4X0Ilqhav5hcs5apYL7gnPYPeRz0CgYALHCj/Ji8XSsDoF/MhVhnGdIs2P99NNdmo3R2Pv0CuZbDKMU559LJHUvrKS8WkuWRDuKrz1W/EQKApFjDGpdqToZqriUFQzwy7mR3ayIiogzNtHcvbDHx8oFnGY0OFksX/ye0/XGpy2SFxYRwGU98HPYeBvAQQrVjdkzfy7BmXQQ==";

#[derive(Serialize)]
struct Claims<'a> {
    iss: String,
    aud: Value,
    sub: &'a str,
    exp: i64,
    iat: i64,
    nbf: i64,
    jti: &'a str,
    repository_id: &'a str,
}

async fn mount_issuer_with(server: &MockServer, keys: Value, content_type: &str, cache_control: Option<&str>) {
    let mut discovery = ResponseTemplate::new(200).set_body_raw(
        json!({
            "issuer": secure_origin(&server.uri()),
            "jwks_uri": format!("{}/keys", secure_origin(&server.uri())),
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

fn identity_with_audience(issuer: &str, kid: Option<&str>, jti: &str, audience: Value, expires_at: i64) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = kid.map(str::to_owned);
    jsonwebtoken::encode(
        &header,
        &Claims {
            iss: secure_origin(issuer),
            aud: audience,
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
    identity_with_audience(issuer, Some(kid), jti, Value::String("peryx".to_owned()), NOW + 600)
}

fn test_verifier(issuer: &str) -> OidcVerifier {
    OidcVerifier::with_http_transport([secure_origin(issuer)], "peryx", transport(issuer)).unwrap()
}

async fn verifier() -> (MockServer, Arc<OidcVerifier>) {
    let server = MockServer::start().await;
    mount_issuer(&server, json!({"keys": [jwk("key-1")]})).await;
    let verifier = Arc::new(test_verifier(&server.uri()));
    (server, verifier)
}

#[async_trait]
trait VerifierExt {
    async fn verify_identity(&self, token: &str, now: i64) -> Result<VerifiedOidcIdentity, OidcVerificationError>;
}

#[async_trait]
impl VerifierExt for OidcVerifier {
    async fn verify_identity(&self, token: &str, now: i64) -> Result<VerifiedOidcIdentity, OidcVerificationError> {
        self.verify(token, "peryx", now).await
    }
}

#[test]
fn test_public_verifier_accepts_an_https_issuer() {
    assert!(OidcVerifier::new(["https://issuer.example".to_owned()], "peryx").is_ok());
}

#[rstest]
#[case::no_issuer(Vec::new(), "peryx")]
#[case::empty_audience(vec!["https://issuer.example".to_owned()], "")]
#[case::malformed(vec!["not a URL".to_owned()], "peryx")]
#[case::http(vec!["http://issuer.example".to_owned()], "peryx")]
#[case::ftp(vec!["ftp://issuer.example".to_owned()], "peryx")]
#[case::credentials(vec!["https://user@issuer.example".to_owned()], "peryx")]
fn test_public_verifier_rejects_invalid_configuration(#[case] issuers: Vec<String>, #[case] audience: &str) {
    assert_eq!(
        OidcVerifier::new(issuers, audience).err(),
        Some(OidcVerificationError::Configuration)
    );
}

#[test]
fn test_injected_transport_preserves_issuer_validation() {
    assert_eq!(
        OidcVerifier::with_http_transport(
            ["http://issuer.example".to_owned()],
            "peryx",
            transport("http://127.0.0.1:1"),
        )
        .err(),
        Some(OidcVerificationError::Configuration)
    );
}

#[tokio::test]
async fn test_verifier_rejects_a_different_expected_audience() {
    let verifier = OidcVerifier::new(["https://issuer.example".to_owned()], "peryx").unwrap();
    assert_eq!(
        verifier.verify("unused", "other", NOW).await,
        Err(OidcVerificationError::InvalidIdentity)
    );
}

#[tokio::test]
async fn test_duplicate_key_ids_reject_the_refresh() {
    let server = MockServer::start().await;
    mount_issuer(&server, json!({"keys": [jwk("same"), jwk("same")]})).await;
    let verifier = test_verifier(&server.uri());
    assert_eq!(
        verifier
            .verify_identity(&identity(&server.uri(), "same", "jti"), NOW)
            .await,
        Err(OidcVerificationError::InvalidIssuerResponse)
    );
}

#[tokio::test]
async fn test_missing_key_id_rejects_the_refresh() {
    let server = MockServer::start().await;
    mount_issuer(
        &server,
        json!({"keys": [{"kty": "RSA", "n": MODULUS, "e": "AQAB", "alg": "RS256"}]}),
    )
    .await;
    let verifier = test_verifier(&server.uri());
    assert_eq!(
        verifier
            .verify_identity(&identity(&server.uri(), "key-1", "jti"), NOW)
            .await,
        Err(OidcVerificationError::InvalidIssuerResponse)
    );
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
    let verifier = test_verifier(&server.uri());
    assert!(
        verifier
            .verify_identity(&identity(&server.uri(), "key-1", "mixed"), NOW)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn test_single_audience_array_is_accepted() {
    let (server, verifier) = verifier().await;
    assert!(
        verifier
            .verify_identity(
                &identity_with_audience(&server.uri(), Some("key-1"), "array", json!(["peryx"]), NOW + 600,),
                NOW,
            )
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
    let verifier = test_verifier(&server.uri());
    assert_eq!(
        verifier
            .verify_identity(&identity(&server.uri(), "key-1", "jti"), NOW)
            .await,
        Err(OidcVerificationError::InvalidIssuerResponse)
    );
}

#[tokio::test]
async fn test_malformed_refresh_keeps_the_working_key() {
    let (server, verifier) = verifier().await;
    verifier
        .verify_identity(&identity(&server.uri(), "key-1", "warm"), NOW)
        .await
        .unwrap();
    server.reset().await;
    mount_issuer(
        &server,
        json!({"keys": [{"kty": "RSA", "n": "!", "e": "AQAB", "kid": "key-1", "alg": "RS256"}]}),
    )
    .await;
    assert!(
        verifier
            .verify_identity(&identity(&server.uri(), "key-1", "stale"), NOW + 121)
            .await
            .is_ok()
    );
    assert!(
        verifier
            .verify_identity(&identity(&server.uri(), "key-1", "cached"), NOW + 122)
            .await
            .is_ok()
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    assert_eq!(
        verifier
            .verify_identity(&identity(&server.uri(), "key-1", "expired"), NOW + HARD_CACHE_SECS + 1)
            .await,
        Err(OidcVerificationError::InvalidIssuerResponse)
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 4);
}

#[tokio::test]
async fn test_bad_signature_does_not_refresh_a_warm_key() {
    let (server, verifier) = verifier().await;
    verifier
        .verify_identity(&identity(&server.uri(), "key-1", "warm"), NOW)
        .await
        .unwrap();
    let mut bad = identity(&server.uri(), "key-1", "bad");
    bad.push('x');
    assert_eq!(
        verifier.verify_identity(&bad, NOW).await,
        Err(OidcVerificationError::InvalidIdentity)
    );
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
    let verifier = test_verifier(&server.uri());
    let token = identity(&server.uri(), "key-1", "cold");
    assert!(matches!(verifier.verify_identity(&token, NOW).await, Err(error) if error.unavailable()));
    assert!(matches!(verifier.verify_identity(&token, NOW).await, Err(error) if error.unavailable()));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_unknown_key_refresh_is_single_flight() {
    let (server, verifier) = verifier().await;
    verifier
        .verify_identity(&identity(&server.uri(), "key-1", "warm"), NOW)
        .await
        .unwrap();
    let unknown = identity(&server.uri(), "key-2", "unknown");
    let (first, second) = tokio::join!(
        verifier.verify_identity(&unknown, NOW + 61),
        verifier.verify_identity(&unknown, NOW + 61)
    );
    assert_eq!(first.unwrap_err(), OidcVerificationError::UnknownKey);
    assert_eq!(second.unwrap_err(), OidcVerificationError::UnknownKey);
    assert_eq!(server.received_requests().await.unwrap().len(), 4);
}

#[tokio::test]
async fn test_claim_time_and_shape_failures_are_closed() {
    let (server, verifier) = verifier().await;
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("key-1".to_owned());
    let key = encoding_key();
    for (jti, claims) in [
        (
            "multi-aud",
            Claims {
                iss: secure_origin(&server.uri()),
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
                iss: secure_origin(&server.uri()),
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
                iss: secure_origin(&server.uri()),
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
                iss: secure_origin(&server.uri()),
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
        assert_eq!(
            verifier.verify_identity(&token, NOW).await,
            Err(OidcVerificationError::InvalidIdentity),
            "{jti}"
        );
    }
}

#[tokio::test]
async fn test_claim_text_bounds_are_closed() {
    let (server, verifier) = verifier().await;
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("key-1".to_owned());
    for (name, sub, jti) in [
        ("subject", "s".repeat(MAX_SUBJECT_BYTES + 1), "jti".to_owned()),
        ("jti", "subject".to_owned(), "j".repeat(MAX_JTI_BYTES + 1)),
    ] {
        let token = jsonwebtoken::encode(
            &header,
            &Claims {
                iss: secure_origin(&server.uri()),
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
        assert_eq!(
            verifier.verify_identity(&token, NOW).await,
            Err(OidcVerificationError::InvalidIdentity),
            "{name}"
        );
    }
}

#[tokio::test]
async fn test_token_size_and_algorithm_are_closed() {
    let (server, verifier) = verifier().await;
    assert_eq!(
        verifier.verify_identity(&"x".repeat(TOKEN_BODY_LIMIT + 1), NOW).await,
        Err(OidcVerificationError::InvalidIdentity)
    );
    let token = jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &Claims {
            iss: secure_origin(&server.uri()),
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
    assert_eq!(
        verifier.verify_identity(&token, NOW).await,
        Err(OidcVerificationError::InvalidIdentity)
    );
}

#[tokio::test]
async fn test_malformed_token_shapes_are_rejected_before_fetching() {
    let verifier = test_verifier("http://issuer.example");
    for (name, token) in [
        ("malformed", "not-a-token".to_owned()),
        (
            "missing-key",
            identity_with_audience("http://issuer.example", None, "missing-key", json!("peryx"), NOW + 600),
        ),
        (
            "unknown-issuer",
            identity("http://unknown.example", "key-1", "unknown-issuer"),
        ),
    ] {
        assert_eq!(
            verifier.verify_identity(&token, NOW).await,
            Err(OidcVerificationError::InvalidIdentity),
            "{name}"
        );
    }
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
                    "jwks_uri": format!("{}/keys", secure_origin(&server.uri())),
                    "id_token_signing_alg_values_supported": ["ES256"]
                })),
        )
        .mount(&server)
        .await;
    let verifier = test_verifier(&server.uri());
    assert_eq!(
        verifier
            .verify_identity(&identity(&server.uri(), "key-1", "jti"), NOW)
            .await,
        Err(OidcVerificationError::InvalidIssuerResponse)
    );
}

#[tokio::test]
async fn test_discovery_rejects_an_invalid_key_set_url() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "issuer": secure_origin(&server.uri()),
                    "jwks_uri": "ftp://issuer.example/keys",
                    "id_token_signing_alg_values_supported": ["RS256"]
                })),
        )
        .mount(&server)
        .await;
    let verifier = test_verifier(&server.uri());
    assert_eq!(
        verifier
            .verify_identity(&identity(&server.uri(), "key-1", "jti"), NOW)
            .await,
        Err(OidcVerificationError::InvalidIssuerResponse)
    );
}

#[tokio::test]
async fn test_unavailable_discovery_endpoint_is_reported() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let issuer = format!("https://{}", listener.local_addr().unwrap());
    drop(listener);
    assert_eq!(
        OidcVerifier::new([issuer.clone()], "peryx")
            .unwrap()
            .verify_identity(&identity(&issuer, "key-1", "unavailable"), NOW)
            .await,
        Err(OidcVerificationError::IssuerUnavailable)
    );
}

#[tokio::test]
async fn test_unavailable_key_set_endpoint_is_reported() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let key_set_url = format!("https://{}/keys", listener.local_addr().unwrap());
    drop(listener);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "issuer": secure_origin(&server.uri()),
                    "jwks_uri": key_set_url,
                    "id_token_signing_alg_values_supported": ["RS256"]
                })),
        )
        .mount(&server)
        .await;
    let verifier = test_verifier(&server.uri());
    assert_eq!(
        verifier
            .verify_identity(&identity(&server.uri(), "key-1", "unavailable"), NOW)
            .await,
        Err(OidcVerificationError::IssuerUnavailable)
    );
}

#[rstest]
#[case::oversized_chunk(
    MalformedDiscoveryBody::OversizedChunked { limit: DISCOVERY_BODY_LIMIT },
    "large",
    OidcVerificationError::InvalidIssuerResponse
)]
#[case::truncated(
    MalformedDiscoveryBody::Truncated,
    "truncated",
    OidcVerificationError::IssuerUnavailable
)]
#[tokio::test]
async fn test_malformed_issuer_body(
    #[case] body: MalformedDiscoveryBody,
    #[case] token_id: &str,
    #[case] expected: OidcVerificationError,
) {
    let server = MalformedDiscoveryServer::start(body);
    let issuer = server.origin();
    assert_eq!(
        test_verifier(&issuer)
            .verify_identity(&identity(&issuer, "key-1", token_id), NOW)
            .await,
        Err(expected)
    );
}

#[rstest]
#[case::configuration(OidcVerificationError::Configuration, false)]
#[case::identity(OidcVerificationError::InvalidIdentity, false)]
#[case::issuer(OidcVerificationError::IssuerUnavailable, true)]
#[case::response(OidcVerificationError::InvalidIssuerResponse, true)]
#[case::key(OidcVerificationError::UnknownKey, true)]
fn test_error_availability(#[case] error: OidcVerificationError, #[case] expected: bool) {
    assert_eq!(error.unavailable(), expected);
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
                    "issuer": secure_origin(&issuer),
                    "jwks_uri": format!("{}/keys", secure_origin(&server.uri())),
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
    let verifier = test_verifier(&issuer);
    assert!(
        verifier
            .verify_identity(&identity(&issuer, "key-1", "path"), NOW)
            .await
            .is_ok()
    );
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
    let verifier = test_verifier(&server.uri());
    verifier
        .verify_identity(&identity(&server.uri(), "key-1", "cold"), NOW)
        .await
        .unwrap();
    verifier
        .verify_identity(&identity(&server.uri(), "key-1", "fresh"), NOW + fresh_for - 1)
        .await
        .unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    verifier
        .verify_identity(&identity(&server.uri(), "key-1", "refresh"), NOW + fresh_for)
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
    let result = test_verifier(&server.uri())
        .verify_identity(&identity(&server.uri(), "key-1", "content-type"), NOW)
        .await;
    if accepted {
        assert_eq!(
            result.unwrap(),
            VerifiedOidcIdentity {
                issuer: secure_origin(&server.uri()),
                audience: "peryx".to_owned(),
                subject: "repo:org/app:ref:refs/heads/main".to_owned(),
                expires_at: NOW + 600,
                token_id: "content-type".to_owned(),
                claims: [("repository_id".to_owned(), json!("42"))].into(),
            }
        );
    } else {
        assert!(matches!(result, Err(OidcVerificationError::InvalidIssuerResponse)));
    }
}
