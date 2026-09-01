use std::sync::Arc;

use async_trait::async_trait;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use rstest::rstest;
use serde::Serialize;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::tests::oidc_http::{
    MAX_DISCOVERY_BYTES, MAX_JWKS_BYTES, TestHttpServer, TestResponseBody, padded_json, routed_transport,
    secure_origin, transport,
};

const NOW: i64 = 2_000_000_000;
const HARD_CACHE_SECS: i64 = 3_600;
const MAX_MACHINE_TOKEN_BYTES: usize = 32_768;
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
    #[serde(skip_serializing_if = "str::is_empty")]
    padding: &'a str,
}

fn claims<'a>(issuer: &str, subject: &'a str, token_id: &'a str, padding: &'a str) -> Claims<'a> {
    Claims {
        iss: secure_origin(issuer),
        aud: json!("peryx"),
        sub: subject,
        exp: NOW + 600,
        iat: NOW,
        nbf: NOW,
        jti: token_id,
        repository_id: "42",
        padding,
    }
}

async fn mount_issuer_with(server: &MockServer, keys: Value, content_type: &str, cache_control: &[&str]) {
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
    for line in cache_control {
        discovery = discovery.append_header("cache-control", *line);
        jwks = jwks.append_header("cache-control", *line);
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
    mount_issuer_with(server, keys, "application/json", &["max-age=120"]).await;
}

/// Replace the mounted issuer with one that fails every discovery request.
async fn mount_outage(server: &MockServer) {
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(500))
        .mount(server)
        .await;
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
    let mut claims = claims(issuer, "repo:org/app:ref:refs/heads/main", jti, "");
    claims.aud = audience;
    claims.exp = expires_at;
    signed_identity(kid, &claims)
}

fn signed_identity(kid: Option<&str>, claims: &Claims<'_>) -> String {
    signed_identity_with_type(kid, None, claims)
}

fn signed_identity_with_type(kid: Option<&str>, token_type: Option<&str>, claims: &Claims<'_>) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = kid.map(str::to_owned);
    header.typ = token_type.map(str::to_owned);
    jsonwebtoken::encode(&header, claims, &encoding_key()).unwrap()
}

fn identity_with_length(issuer: &str, length: usize) -> String {
    (0..4)
        .find_map(|type_length| {
            let token_type = "x".repeat(type_length);
            let base = signed_identity_with_type(
                Some("key-1"),
                Some(&token_type),
                &claims(issuer, "repo:org/app:ref:refs/heads/main", "sized", ""),
            );
            let estimate = (length - base.len()) * 3 / 4;
            (estimate.saturating_sub(32)..=estimate + 32)
                .map(|padding| {
                    let padding = "x".repeat(padding);
                    signed_identity_with_type(
                        Some("key-1"),
                        Some(&token_type),
                        &claims(issuer, "repo:org/app:ref:refs/heads/main", "sized", &padding),
                    )
                })
                .find(|token| token.len() == length)
        })
        .expect("JWT padding reaches the requested length")
}

fn identity(issuer: &str, kid: &str, jti: &str) -> String {
    identity_with_audience(issuer, Some(kid), jti, Value::String("peryx".to_owned()), NOW + 600)
}

fn test_verifier(issuer: &str) -> OidcVerifier {
    OidcVerifier::new([secure_origin(issuer)], "peryx", transport(issuer)).unwrap()
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
    assert!(
        OidcVerifier::new(
            ["https://issuer.example".to_owned()],
            "peryx",
            transport("https://issuer.example")
        )
        .is_ok()
    );
}

#[rstest]
#[case::no_issuer(Vec::new(), "peryx")]
#[case::empty_audience(vec!["https://issuer.example".to_owned()], "")]
#[case::malformed(vec!["not a URL".to_owned()], "peryx")]
#[case::http(vec!["http://issuer.example".to_owned()], "peryx")]
#[case::ftp(vec!["ftp://issuer.example".to_owned()], "peryx")]
#[case::credentials(vec!["https://user@issuer.example".to_owned()], "peryx")]
#[case::password(vec!["https://user:secret@issuer.example".to_owned()], "peryx")]
#[case::query(vec!["https://issuer.example?tenant=one".to_owned()], "peryx")]
#[case::fragment(vec!["https://issuer.example#tenant".to_owned()], "peryx")]
fn test_public_verifier_rejects_invalid_configuration(#[case] issuers: Vec<String>, #[case] audience: &str) {
    assert_eq!(
        OidcVerifier::new(issuers, audience, transport("https://issuer.example")).err(),
        Some(OidcVerificationError::Configuration)
    );
}

#[tokio::test]
async fn test_verifier_rejects_a_different_expected_audience() {
    let verifier = OidcVerifier::new(
        ["https://issuer.example".to_owned()],
        "peryx",
        transport("https://issuer.example"),
    )
    .unwrap();
    assert_eq!(
        verifier.verify("unused", "other", NOW).await,
        Err(OidcVerificationError::InvalidIdentity)
    );
}

#[rstest]
#[case::duplicate(json!({"keys": [jwk("same"), jwk("same")]}))]
#[case::no_id(json!({"keys": [{"kty": "RSA", "n": MODULUS, "e": "AQAB", "alg": "RS256"}]}))]
#[case::bad_modulus(json!({"keys": [{"kty": "RSA", "n": "!", "e": "AQAB", "kid": "broken", "alg": "RS256"}]}))]
#[tokio::test]
async fn test_unusable_key_sets_reject_the_refresh(#[case] keys: Value) {
    let server = MockServer::start().await;
    mount_issuer(&server, keys).await;
    let verifier = test_verifier(&server.uri());
    assert_eq!(
        verifier
            .verify_identity(&identity(&server.uri(), "broken", "jti"), NOW)
            .await,
        Err(OidcVerificationError::InvalidIssuerResponse)
    );
}

#[tokio::test]
async fn test_unusable_entries_do_not_hide_a_usable_key() {
    let server = MockServer::start().await;
    mount_issuer(
        &server,
        json!({"keys": [
            {"kty": "oct", "k": "c2VjcmV0", "kid": "symmetric", "alg": "HS256"},
            {"kty": "RSA", "n": MODULUS, "e": "AQAB", "kid": "signing-only", "alg": "RS256", "use": "sig", "key_ops": ["sign"]},
            {"kty": "RSA", "n": MODULUS, "e": "AQAB", "alg": "RS256"},
            {"kty": "RSA", "n": "!", "e": "AQAB", "kid": "key-1", "alg": "RS256", "use": "sig"},
            {"kty": "RSA", "n": MODULUS, "e": "AQAB", "kid": "key-1", "alg": "RS256", "use": "sig", "key_ops": ["verify"]}
        ]}),
    )
    .await;
    let verifier = test_verifier(&server.uri());
    assert_eq!(
        verifier
            .verify_identity(&identity(&server.uri(), "key-1", "mixed"), NOW)
            .await
            .unwrap(),
        VerifiedOidcIdentity {
            issuer: secure_origin(&server.uri()),
            audience: "peryx".to_owned(),
            subject: "repo:org/app:ref:refs/heads/main".to_owned(),
            expires_at: NOW + 600,
            token_id: "mixed".to_owned(),
            claims: [("repository_id".to_owned(), json!("42"))].into(),
        }
    );
}

#[tokio::test]
async fn test_unusable_matching_key_remains_unknown() {
    let server = MockServer::start().await;
    mount_issuer(
        &server,
        json!({"keys": [
            {"kty": "RSA", "n": "!", "e": "AQAB", "kid": "broken", "alg": "RS256", "use": "sig"},
            jwk("key-1")
        ]}),
    )
    .await;
    let verifier = test_verifier(&server.uri());
    assert_eq!(
        verifier
            .verify_identity(&identity(&server.uri(), "broken", "unknown"), NOW)
            .await,
        Err(OidcVerificationError::UnknownKey)
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

#[rstest]
#[case::symmetric(json!({"kty": "oct", "k": "c2VjcmV0", "kid": "symmetric", "alg": "HS256"}))]
#[case::wrong_algorithm(json!({"kty": "RSA", "n": MODULUS, "e": "AQAB", "kid": "key-1", "alg": "HS256"}))]
#[tokio::test]
async fn test_key_set_without_a_usable_key_is_rejected(#[case] key: Value) {
    let server = MockServer::start().await;
    mount_issuer(&server, json!({"keys": [key]})).await;
    let verifier = test_verifier(&server.uri());
    assert_eq!(
        verifier
            .verify_identity(&identity(&server.uri(), "key-1", "jti"), NOW)
            .await,
        Err(OidcVerificationError::InvalidIssuerResponse)
    );
}

#[tokio::test]
async fn test_maximum_jwks_body_is_accepted() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "issuer": secure_origin(&server.uri()),
                    "jwks_uri": format!("{}/keys", secure_origin(&server.uri())),
                    "id_token_signing_alg_values_supported": ["RS256"]
                })),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/keys"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            padded_json(json!({"keys": [jwk("key-1")]}), MAX_JWKS_BYTES),
            "application/json",
        ))
        .mount(&server)
        .await;

    assert!(
        test_verifier(&server.uri())
            .verify_identity(&identity(&server.uri(), "key-1", "maximum-jwks"), NOW)
            .await
            .is_ok()
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
async fn test_signing_key_rotation_refreshes_a_fresh_cache() {
    let (server, verifier) = verifier().await;
    verifier
        .verify_identity(&identity(&server.uri(), "key-1", "warm"), NOW)
        .await
        .unwrap();
    server.reset().await;
    mount_issuer(&server, json!({"keys": [jwk("key-2")]})).await;

    assert!(
        verifier
            .verify_identity(&identity(&server.uri(), "key-2", "rotated"), NOW + 1)
            .await
            .is_ok()
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
    assert!(matches!(verifier.verify_identity(&token, NOW + 59).await, Err(error) if error.unavailable()));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert!(matches!(verifier.verify_identity(&token, NOW + 60).await, Err(error) if error.unavailable()));
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
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
        verifier.verify_identity(&unknown, NOW + 1),
        verifier.verify_identity(&unknown, NOW + 1)
    );
    assert_eq!(first.unwrap_err(), OidcVerificationError::UnknownKey);
    assert_eq!(second.unwrap_err(), OidcVerificationError::UnknownKey);
    assert_eq!(server.received_requests().await.unwrap().len(), 4);
}

#[tokio::test]
async fn test_unknown_key_refresh_is_rate_limited() {
    let (server, verifier) = verifier().await;
    verifier
        .verify_identity(&identity(&server.uri(), "key-1", "warm"), NOW)
        .await
        .unwrap();
    let unknown = identity(&server.uri(), "key-2", "unknown");
    assert_eq!(
        verifier.verify_identity(&unknown, NOW + 1).await,
        Err(OidcVerificationError::UnknownKey)
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 4);
    assert_eq!(
        verifier.verify_identity(&unknown, NOW + 60).await,
        Err(OidcVerificationError::UnknownKey)
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 4);
    assert_eq!(
        verifier.verify_identity(&unknown, NOW + 61).await,
        Err(OidcVerificationError::UnknownKey)
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 6);
}

#[rstest]
#[case::fresh(&["max-age=120"], OidcVerificationError::UnknownKey, true)]
#[case::revalidation_required(
    &["max-age=0, must-revalidate"],
    OidcVerificationError::InvalidIssuerResponse,
    false
)]
#[tokio::test]
async fn test_failed_key_refresh_respects_cached_key_policy(
    #[case] cache_control: &[&str],
    #[case] miss_error: OidcVerificationError,
    #[case] accepted: bool,
) {
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
        .verify_identity(&identity(&server.uri(), "key-1", "warm"), NOW)
        .await
        .unwrap();
    mount_outage(&server).await;
    assert_eq!(
        verifier
            .verify_identity(&identity(&server.uri(), "key-2", "miss"), NOW + 1)
            .await,
        Err(miss_error)
    );

    assert_eq!(
        verifier
            .verify_identity(&identity(&server.uri(), "key-1", "known"), NOW + 2)
            .await
            .is_ok(),
        accepted
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_multiple_audiences_are_rejected() {
    let (server, verifier) = verifier().await;
    let mut claims = claims(&server.uri(), "repo:org/app:x", "multiple-audiences", "");
    claims.aud = json!(["peryx", "other"]);
    let token = signed_identity(Some("key-1"), &claims);

    assert_eq!(
        verifier.verify_identity(&token, NOW).await,
        Err(OidcVerificationError::InvalidIdentity)
    );
}

#[rstest]
#[case::future_issued_at(NOW + 1, NOW, NOW + 60, false)]
#[case::future_not_before(NOW, NOW + 1, NOW + 60, false)]
#[case::maximum_lifetime(NOW, NOW, NOW + 3_600, true)]
#[case::excessive_lifetime(NOW, NOW, NOW + 3_601, false)]
#[case::zero_lifetime(NOW, NOW, NOW, false)]
#[case::overflow(i64::MIN, i64::MIN, i64::MAX, false)]
#[tokio::test]
async fn test_claim_time_bounds(
    #[case] issued_at: i64,
    #[case] not_before: i64,
    #[case] expires_at: i64,
    #[case] accepted: bool,
) {
    let (server, verifier) = verifier().await;
    let mut claims = claims(&server.uri(), "repo:org/app:x", "time-boundary", "");
    claims.exp = expires_at;
    claims.iat = issued_at;
    claims.nbf = not_before;
    let token = signed_identity(Some("key-1"), &claims);

    assert_eq!(verifier.verify_identity(&token, NOW).await.is_ok(), accepted);
}

#[rstest]
#[case::maximum_subject(2_048, 3, true)]
#[case::oversized_subject(2_049, 3, false)]
#[case::maximum_token_id(7, 256, true)]
#[case::oversized_token_id(7, 257, false)]
#[tokio::test]
async fn test_claim_text_bounds(#[case] subject_length: usize, #[case] token_id_length: usize, #[case] accepted: bool) {
    let (server, verifier) = verifier().await;
    let subject = "s".repeat(subject_length);
    let token_id = "j".repeat(token_id_length);
    let token = signed_identity(Some("key-1"), &claims(&server.uri(), &subject, &token_id, ""));

    assert_eq!(verifier.verify_identity(&token, NOW).await.is_ok(), accepted);
}

#[rstest]
#[case::maximum(MAX_MACHINE_TOKEN_BYTES, true)]
#[case::oversized(MAX_MACHINE_TOKEN_BYTES + 1, false)]
#[tokio::test]
async fn test_token_size_bound(#[case] length: usize, #[case] accepted: bool) {
    let (server, verifier) = verifier().await;
    let token = identity_with_length(&server.uri(), length);

    assert_eq!(verifier.verify_identity(&token, NOW).await.is_ok(), accepted);
}

#[tokio::test]
async fn test_token_algorithm_is_enforced() {
    let (server, verifier) = verifier().await;
    let token = jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &claims(&server.uri(), "repo:org/app:x", "wrong-algorithm", ""),
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

#[rstest]
#[case::issuer(false, true)]
#[case::algorithm(true, false)]
#[tokio::test]
async fn test_discovery_must_repeat_the_issuer_and_algorithm(
    #[case] issuer_matches: bool,
    #[case] supports_rs256: bool,
) {
    let server = MockServer::start().await;
    let issuer = if issuer_matches {
        secure_origin(&server.uri())
    } else {
        "https://other.example".to_owned()
    };
    let algorithm = if supports_rs256 { "RS256" } else { "ES256" };
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "issuer": issuer,
                    "jwks_uri": format!("{}/keys", secure_origin(&server.uri())),
                    "id_token_signing_alg_values_supported": [algorithm]
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

async fn mount_issuer_with_keys_at(server: &MockServer, jwks_uri: &str) {
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .insert_header("cache-control", "max-age=120")
                .set_body_json(json!({
                    "issuer": secure_origin(&server.uri()),
                    "jwks_uri": jwks_uri,
                    "id_token_signing_alg_values_supported": ["RS256"]
                })),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/keys"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .insert_header("cache-control", "max-age=120")
                .set_body_json(json!({"keys": [jwk("key-1")]})),
        )
        .mount(server)
        .await;
}

/// Discovery names the key endpoint, so a host the outbound policy refuses must not be connected
/// to even though the key set behind it would answer.
#[tokio::test]
async fn test_key_set_host_the_policy_refuses_is_rejected() {
    let server = MockServer::start().await;
    mount_issuer_with_keys_at(&server, "https://metadata.internal/keys").await;
    let verifier = OidcVerifier::new(
        [secure_origin(&server.uri())],
        "peryx",
        routed_transport(&server.uri(), &["https://metadata.internal"], &["metadata.internal"]),
    )
    .unwrap();

    assert_eq!(
        verifier
            .verify_identity(&identity(&server.uri(), "key-1", "refused"), NOW)
            .await,
        Err(OidcVerificationError::BlockedDestination)
    );
}

/// `OpenID` Connect Discovery permits a key endpoint on a host other than the issuer, so a permitted
/// one still verifies.
#[tokio::test]
async fn test_key_set_host_the_policy_permits_still_verifies() {
    let server = MockServer::start().await;
    mount_issuer_with_keys_at(&server, "https://keys.example/keys").await;
    let verifier = OidcVerifier::new(
        [secure_origin(&server.uri())],
        "peryx",
        routed_transport(&server.uri(), &["https://keys.example"], &[]),
    )
    .unwrap();

    let accepted = verifier
        .verify_identity(&identity(&server.uri(), "key-1", "permitted"), NOW)
        .await
        .unwrap();

    assert_eq!(accepted.subject, "repo:org/app:ref:refs/heads/main");
}

#[tokio::test]
async fn test_unavailable_discovery_endpoint_is_reported() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let issuer = format!("https://{}", listener.local_addr().unwrap());
    drop(listener);
    assert_eq!(
        test_verifier(&issuer)
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
#[case::exact_chunk(
    TestResponseBody::ExactChunked { limit: MAX_DISCOVERY_BYTES },
    "exact-chunk",
    OidcVerificationError::IssuerUnavailable
)]
#[case::oversized_chunk(
    TestResponseBody::OversizedChunked { limit: MAX_DISCOVERY_BYTES },
    "large",
    OidcVerificationError::InvalidIssuerResponse
)]
#[case::exact_length(
    TestResponseBody::ExactContentLength { limit: MAX_DISCOVERY_BYTES },
    "exact-length",
    OidcVerificationError::IssuerUnavailable
)]
#[case::oversized_length(
    TestResponseBody::OversizedContentLength { limit: MAX_DISCOVERY_BYTES },
    "large-length",
    OidcVerificationError::InvalidIssuerResponse
)]
#[case::truncated(TestResponseBody::Truncated, "truncated", OidcVerificationError::IssuerUnavailable)]
#[tokio::test]
async fn test_malformed_issuer_body(
    #[case] body: TestResponseBody,
    #[case] token_id: &str,
    #[case] expected: OidcVerificationError,
) {
    let server = TestHttpServer::start(body);
    let issuer = server.origin();
    assert_eq!(
        test_verifier(&issuer)
            .verify_identity(&identity(&issuer, "key-1", token_id), NOW)
            .await,
        Err(expected)
    );
}

/// The refusal has to name a policy an operator can change, not read as a provider outage.
#[test]
fn test_blocked_destination_reports_the_outbound_policy() {
    assert_eq!(
        OidcVerificationError::BlockedDestination.to_string(),
        "the issuer named a destination the outbound policy refuses"
    );
}

#[rstest]
#[case::configuration(OidcVerificationError::Configuration, false)]
#[case::identity(OidcVerificationError::InvalidIdentity, false)]
#[case::issuer(OidcVerificationError::IssuerUnavailable, true)]
#[case::response(OidcVerificationError::InvalidIssuerResponse, true)]
#[case::blocked(OidcVerificationError::BlockedDestination, true)]
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
#[case::absent(&[], 300)]
#[case::below_the_backoff(&["max-age=30"], 30)]
#[case::clamped(&["max-age=100000"], 900)]
#[case::mixed_case(&["MAX-AGE=45"], 45)]
#[case::qualified(&["community=\"x, max-age=1\", max-age=45"], 45)]
#[case::separate_field_lines(&["max-age=45", "proxy-revalidate"], 45)]
#[tokio::test]
async fn test_cache_control_sets_refresh_time(#[case] cache_control: &[&str], #[case] fresh_for: i64) {
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

/// A response the provider forbids storing verifies the token that fetched it and nothing later.
#[rstest]
#[case::no_store(&["no-store"])]
#[case::private(&["max-age=600", "Private"])]
#[tokio::test]
async fn test_unstorable_jwks_leaves_no_cached_key(#[case] cache_control: &[&str]) {
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
        .verify_identity(&identity(&server.uri(), "key-1", "fetched"), NOW)
        .await
        .unwrap();
    mount_outage(&server).await;
    assert_eq!(
        verifier
            .verify_identity(&identity(&server.uri(), "key-1", "later"), NOW + 1)
            .await,
        Err(OidcVerificationError::InvalidIssuerResponse)
    );
}

#[rstest]
#[case::unqualified(&["no-cache"])]
#[case::qualified(&["no-cache=\"Set-Cookie\", max-age=600"])]
#[case::mixed_case(&["No-Cache"])]
#[tokio::test]
async fn test_no_cache_key_needs_a_successful_revalidation(#[case] cache_control: &[&str]) {
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
        .verify_identity(&identity(&server.uri(), "key-1", "fetched"), NOW)
        .await
        .unwrap();
    verifier
        .verify_identity(&identity(&server.uri(), "key-1", "revalidated"), NOW + 1)
        .await
        .unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 4);
    mount_outage(&server).await;
    assert_eq!(
        verifier
            .verify_identity(&identity(&server.uri(), "key-1", "unvalidated"), NOW + 2)
            .await,
        Err(OidcVerificationError::InvalidIssuerResponse)
    );
}

/// `max-age=0` demands revalidation, and alone it still allows the bounded stale window;
/// `must-revalidate` withdraws that window.
#[rstest]
#[case::stale_allowed(&["max-age=0"], Ok(()))]
#[case::stale_forbidden(&["max-age=0, must-revalidate"], Err(OidcVerificationError::InvalidIssuerResponse))]
#[tokio::test]
async fn test_zero_max_age_revalidates(
    #[case] cache_control: &[&str],
    #[case] expected: Result<(), OidcVerificationError>,
) {
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
        .verify_identity(&identity(&server.uri(), "key-1", "fetched"), NOW)
        .await
        .unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    mount_outage(&server).await;
    assert_eq!(
        verifier
            .verify_identity(&identity(&server.uri(), "key-1", "stale"), NOW + 1)
            .await
            .map(|_| ()),
        expected
    );
}

#[tokio::test]
async fn test_zero_max_age_stale_window_ends_at_the_hard_limit() {
    let server = MockServer::start().await;
    mount_issuer_with(
        &server,
        json!({"keys": [jwk("key-1")]}),
        "application/json",
        &["max-age=0"],
    )
    .await;
    let verifier = test_verifier(&server.uri());
    verifier
        .verify_identity(&identity(&server.uri(), "key-1", "fetched"), NOW)
        .await
        .unwrap();
    mount_outage(&server).await;
    assert_eq!(
        verifier
            .verify_identity(&identity(&server.uri(), "key-1", "expired"), NOW + HARD_CACHE_SECS)
            .await,
        Err(OidcVerificationError::InvalidIssuerResponse)
    );
}

#[rstest]
#[case::json("application/json; charset=utf-8", true)]
#[case::structured("application/jwk-set+json", true)]
#[case::case_insensitive("Application/JSON", true)]
#[case::application_non_json("application/text", false)]
#[case::non_application_json("text/example+json", false)]
#[case::wrong_type("text/json", false)]
#[tokio::test]
async fn test_discovery_content_type_is_enforced(#[case] content_type: &str, #[case] accepted: bool) {
    let server = MockServer::start().await;
    mount_issuer_with(&server, json!({"keys": [jwk("key-1")]}), content_type, &["max-age=120"]).await;
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
