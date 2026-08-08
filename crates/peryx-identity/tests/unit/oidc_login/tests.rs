use std::sync::Mutex;

use jsonwebtoken::{EncodingKey, Header};
use rstest::rstest;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::{ExternalIdentityResolution, ExternalLinkRequest, ServerUser, UserId, UserState};

const NOW: i64 = 2_000_000_000;
const MODULUS: &str = "yRE6rHuNR0QbHO3H3Kt2pOKGVhQqGZXInOduQNxXzuKlvQTLUTv4l4sggh5_CYYi_cvI-SXVT9kPWSKXxJXBXd_4LkvcPuUakBoAkfh-eiFVMh2VrUyWyj3MFl0HTVF9KwRXLAcwkREiS3npThHRyIxuy0ZMeZfxVL5arMhw1SRELB8HoGfG_AtH89BIE9jDBHZ9dLelK9a184zAf8LwoPLxvJb3Il5nncqPcSfKDDodMFBIMc4lQzDKL5gvmiXLXB1AGLm8KBjfE8s3L5xqi-yUod-j8MtvIj812dkS4QMiRVN_by2h3ZY8LYVGrqZXZTcgn2ujn8uKjXLZVD5TdQ";
const PRIVATE_KEY_DER: &str = "MIIEpAIBAAKCAQEAyRE6rHuNR0QbHO3H3Kt2pOKGVhQqGZXInOduQNxXzuKlvQTLUTv4l4sggh5/CYYi/cvI+SXVT9kPWSKXxJXBXd/4LkvcPuUakBoAkfh+eiFVMh2VrUyWyj3MFl0HTVF9KwRXLAcwkREiS3npThHRyIxuy0ZMeZfxVL5arMhw1SRELB8HoGfG/AtH89BIE9jDBHZ9dLelK9a184zAf8LwoPLxvJb3Il5nncqPcSfKDDodMFBIMc4lQzDKL5gvmiXLXB1AGLm8KBjfE8s3L5xqi+yUod+j8MtvIj812dkS4QMiRVN/by2h3ZY8LYVGrqZXZTcgn2ujn8uKjXLZVD5TdQIDAQABAoIBAHREk0I0O9DvECKdWUpAmF3mY7oY9PNQiu44Yaf+AoSuyRpRUGTMIgc3u3eivOE8ALX0BmYUO5JtuRNZDpvt4SAwqCnVUinIf6C+eH/wSurCpapSM0BAHp4aOA7igptyOMgMPYBHNA1e9A7jE0dCxKWMl3DSWNyjQTk4zeRGEAEfbNjHrq6YCtjHSZSLmWiG80hnfnYos9hOr5JnLnyS7ZmFE/5P3XVrxLc/tQ5zum0R4cbrgzHiQP5RgfxGJaEi7XcgherCCOgurJSSbYH29Gz8u5fFbS+Yg8s+OiCss3cs1rSgJ9/eHZuzGEdUZVARH6hVMjSuwvqVTFaE8AgtleECgYEA+uLMn4kNqHlJS2A5uAnCkj90ZxEtNm3E8hAxUrhssktY5XSOAPBlxyf5RuRGIImGtUVIr4HuJSa5TX48n3Vdt9MYCprO/iYl6moNRSPt5qowIIOJmIjY2mqPDfDt/zw+fcDD3lmCJrFlzcnh0uea1CohxEbQnL3cypeLt+WbU6kCgYEAzSp19m1ajieFkqgoB0YTpt/OroDx38vvI5unInJlEeOjQ+oIAQdN2wpxBvTrRorMU6P07mFUbt1j+Co6CbNiw+X8HcCaqYLR5clbJOOWNR36PuzOpQLkfK8woupBxzW9B8gZmY8rB1mbJ+/WTPrEJy6YGmIEBkWylQ2VpW8O4O0CgYEApdbvvfFBlwD9YxbrcGz7MeNCFbMz+MucqQntIKoKJ91ImPxvtc0y6e/Rhnv0oyNlaUOwJVu0yNgNG117w0g4t/+Q38mvVC5xV7/cn7x9UMFk6MkqVir3dYGEqIl/OP1grY2Tq9HtB5iyG9L8NIamQOLMyUqqMUILxdthHyFmiGkCgYEAn9+PjpjGMPHxL0gj8Q8VbzsFtou6b1deIRRA2CHmSltltR1gYVTMwXxQeUhPMmgkMqUXzs4/WijgpthY44hK1TaZEKIuoxrS70nJ4WQLf5a9k1065fDsFZD6yGjdGxvwEmlGMZgTwqV7t1I4X0Ilqhav5hcs5apYL7gnPYPeRz0CgYALHCj/Ji8XSsDoF/MhVhnGdIs2P99NNdmo3R2Pv0CuZbDKMU559LJHUvrKS8WkuWRDuKrz1W/EQKApFjDGpdqToZqriUFQzwy7mR3ayIiogzNtHcvbDHx8oFnGY0OFksX/ye0/XGpy2SFxYRwGU98HPYeBvAQQrVjdkzfy7BmXQQ==";

fn settings(issuer: &str) -> OidcProviderSettings {
    OidcProviderSettings {
        id: ProviderId::new("corporate").unwrap(),
        issuer: Url::parse(issuer).unwrap(),
        client_id: "peryx-web".to_owned(),
        client_secret: Some("s3cret".to_owned()),
        redirect_uri: Url::parse("https://registry.example/oidc/corporate/callback").unwrap(),
        scopes: vec!["email".to_owned(), "groups".to_owned()],
        subject_claim: "sub".to_owned(),
        display_name_claim: "name".to_owned(),
        groups_claim: Some("groups".to_owned()),
        clock_skew: Duration::from_mins(1),
        request_timeout: Duration::from_secs(5),
    }
}

fn provider(issuer: &str) -> OidcLoginProvider {
    OidcLoginProvider::build(settings(issuer), true).unwrap()
}

fn encoding_key() -> EncodingKey {
    use base64::engine::general_purpose::STANDARD;
    EncodingKey::from_rsa_der(&STANDARD.decode(PRIVATE_KEY_DER).unwrap())
}

fn jwk(kid: &str) -> Value {
    json!({"kty": "RSA", "n": MODULUS, "e": "AQAB", "kid": kid, "alg": "RS256", "use": "sig"})
}

fn mint(kid: &str, claims: &Value) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(kid.to_owned());
    jsonwebtoken::encode(&header, claims, &encoding_key()).unwrap()
}

fn issuer(server: &MockServer) -> String {
    format!("{}/", server.uri())
}

fn base_claims(server: &MockServer) -> Value {
    json!({
        "iss": issuer(server),
        "aud": "peryx-web",
        "exp": NOW + 7200,
        "iat": NOW,
        "nonce": "nonce-abc",
        "sub": "subject-123",
        "name": "Ada Lovelace",
        "groups": ["dev", "ops"],
    })
}

fn pending() -> PendingLogin {
    PendingLogin {
        state: "state-abc".to_owned(),
        nonce: "nonce-abc".to_owned(),
        verifier: "verifier-abc".to_owned(),
        challenge: "challenge-abc".to_owned(),
    }
}

fn response() -> CallbackResponse {
    CallbackResponse {
        state: "state-abc".to_owned(),
        code: "auth-code".to_owned(),
    }
}

async fn mount_metadata(server: &MockServer, keys: Value) {
    mount_discovery(
        server,
        json!({
            "issuer": issuer(server),
            "authorization_endpoint": format!("{}/authorize", server.uri()),
            "token_endpoint": format!("{}/token", server.uri()),
            "jwks_uri": format!("{}/jwks", server.uri()),
            "id_token_signing_alg_values_supported": ["RS256"],
        }),
        "application/json",
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/jwks"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(keys),
        )
        .mount(server)
        .await;
}

async fn mount_discovery(server: &MockServer, body: Value, content_type: &str) {
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("cache-control", "max-age=120")
                .set_body_raw(body.to_string(), content_type),
        )
        .mount(server)
        .await;
}

async fn mount_token(server: &MockServer, body: Value) {
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(body),
        )
        .mount(server)
        .await;
}

async fn ready() -> (MockServer, OidcLoginProvider) {
    let server = MockServer::start().await;
    mount_metadata(&server, json!({"keys": [jwk("key-1")]})).await;
    let provider = provider(&server.uri());
    (server, provider)
}

#[tokio::test]
async fn test_authorization_binds_state_nonce_and_pkce() {
    let (_server, provider) = ready().await;
    let authorization = provider.authorization(NOW).await.unwrap();
    let query: std::collections::HashMap<_, _> = authorization.redirect_url.query_pairs().into_owned().collect();
    assert_eq!(query["response_type"], "code");
    assert_eq!(query["client_id"], "peryx-web");
    assert_eq!(
        query["redirect_uri"],
        "https://registry.example/oidc/corporate/callback"
    );
    assert_eq!(query["code_challenge_method"], "S256");
    assert_eq!(query["state"], authorization.pending.state);
    assert_eq!(query["nonce"], authorization.pending.nonce);
    assert_eq!(query["code_challenge"], pkce_challenge(&authorization.pending.verifier));
    assert_eq!(query["code_challenge"], authorization.pending.challenge);
    assert!(query["scope"].split(' ').eq(["openid", "email", "groups"]));
    assert_ne!(authorization.pending.state, authorization.pending.nonce);
}

#[tokio::test]
async fn test_valid_response_yields_the_linked_subject() {
    let (server, provider) = ready().await;
    let authorization = provider.authorization(NOW).await.unwrap();
    let mut claims = base_claims(&server);
    claims["nonce"] = authorization.pending.nonce.clone().into();
    mount_token(
        &server,
        json!({"id_token": mint("key-1", &claims), "token_type": "Bearer"}),
    )
    .await;
    let callback = CallbackResponse {
        state: authorization.pending.state.clone(),
        code: "auth-code".to_owned(),
    };
    let login = provider.callback(&callback, &authorization.pending, NOW).await.unwrap();
    assert_eq!(login.identity.subject.as_str(), "subject-123");
    assert_eq!(login.identity.provider.as_str(), "corporate");
    assert_eq!(login.display_name.display(), "Ada Lovelace");
    assert_eq!(
        login.groups.iter().map(ExternalGroup::as_str).collect::<Vec<_>>(),
        vec!["dev", "ops"]
    );
}

#[tokio::test]
async fn test_callback_rejects_a_mismatched_state() {
    let provider = provider("https://issuer.example");
    let callback = CallbackResponse {
        state: "forged".to_owned(),
        code: "auth-code".to_owned(),
    };
    assert!(matches!(
        provider.callback(&callback, &pending(), NOW).await,
        Err(OidcProviderError::StateMismatch)
    ));
}

#[rstest]
#[case::wrong_issuer(json!("https://evil.example"), "iss")]
#[case::wrong_audience(json!("other-client"), "aud")]
#[case::expired(json!(NOW - 3600), "exp")]
#[case::future_issued(json!(NOW + 7200), "iat")]
#[tokio::test]
async fn test_id_token_registered_claims_are_enforced(#[case] value: Value, #[case] claim: &str) {
    let server = MockServer::start().await;
    mount_metadata(&server, json!({"keys": [jwk("key-1")]})).await;
    let provider = provider(&server.uri());
    let mut claims = base_claims(&server);
    claims[claim] = value;
    mount_token(
        &server,
        json!({"id_token": mint("key-1", &claims), "token_type": "Bearer"}),
    )
    .await;
    assert!(matches!(
        provider.callback(&response(), &pending(), NOW).await,
        Err(OidcProviderError::InvalidToken)
    ));
}

#[tokio::test]
async fn test_nonce_mismatch_fails_the_login() {
    let (server, provider) = ready().await;
    let mut claims = base_claims(&server);
    claims["nonce"] = "different".into();
    mount_token(
        &server,
        json!({"id_token": mint("key-1", &claims), "token_type": "Bearer"}),
    )
    .await;
    assert!(matches!(
        provider.callback(&response(), &pending(), NOW).await,
        Err(OidcProviderError::InvalidToken)
    ));
}

#[tokio::test]
async fn test_bad_signature_fails_the_login() {
    let (server, provider) = ready().await;
    let mut token = mint("key-1", &base_claims(&server));
    token.push('x');
    mount_token(&server, json!({"id_token": token, "token_type": "Bearer"})).await;
    assert!(matches!(
        provider.callback(&response(), &pending(), NOW).await,
        Err(OidcProviderError::InvalidToken)
    ));
}

#[tokio::test]
async fn test_non_rs256_algorithm_is_rejected() {
    let (server, provider) = ready().await;
    let token = jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &base_claims(&server),
        &EncodingKey::from_secret(b"secret"),
    )
    .unwrap();
    mount_token(&server, json!({"id_token": token, "token_type": "Bearer"})).await;
    assert!(matches!(
        provider.callback(&response(), &pending(), NOW).await,
        Err(OidcProviderError::InvalidToken)
    ));
}

#[tokio::test]
async fn test_missing_key_id_is_rejected() {
    let (server, provider) = ready().await;
    let token = jsonwebtoken::encode(&Header::new(Algorithm::RS256), &base_claims(&server), &encoding_key()).unwrap();
    mount_token(&server, json!({"id_token": token, "token_type": "Bearer"})).await;
    assert!(matches!(
        provider.callback(&response(), &pending(), NOW).await,
        Err(OidcProviderError::InvalidToken)
    ));
}

#[tokio::test]
async fn test_authorized_party_must_match_the_client() {
    let (server, provider) = ready().await;
    let mut claims = base_claims(&server);
    claims["azp"] = "someone-else".into();
    mount_token(
        &server,
        json!({"id_token": mint("key-1", &claims), "token_type": "Bearer"}),
    )
    .await;
    assert!(matches!(
        provider.callback(&response(), &pending(), NOW).await,
        Err(OidcProviderError::InvalidToken)
    ));
}

#[tokio::test]
async fn test_matching_authorized_party_is_accepted() {
    let (server, provider) = ready().await;
    let mut claims = base_claims(&server);
    claims["azp"] = "peryx-web".into();
    mount_token(
        &server,
        json!({"id_token": mint("key-1", &claims), "token_type": "Bearer"}),
    )
    .await;
    assert!(provider.callback(&response(), &pending(), NOW).await.is_ok());
}

#[tokio::test]
async fn test_multiple_audiences_without_authorized_party_are_rejected() {
    let (server, provider) = ready().await;
    let mut claims = base_claims(&server);
    claims["aud"] = json!(["peryx-web", "other-client"]);
    mount_token(
        &server,
        json!({"id_token": mint("key-1", &claims), "token_type": "Bearer"}),
    )
    .await;
    assert!(matches!(
        provider.callback(&response(), &pending(), NOW).await,
        Err(OidcProviderError::InvalidToken)
    ));
}

#[tokio::test]
async fn test_multiple_audiences_with_matching_authorized_party_are_accepted() {
    let (server, provider) = ready().await;
    let mut claims = base_claims(&server);
    claims["aud"] = json!(["peryx-web", "other-client"]);
    claims["azp"] = "peryx-web".into();
    mount_token(
        &server,
        json!({"id_token": mint("key-1", &claims), "token_type": "Bearer"}),
    )
    .await;
    assert!(provider.callback(&response(), &pending(), NOW).await.is_ok());
}

#[tokio::test]
async fn test_single_audience_array_is_accepted() {
    let (server, provider) = ready().await;
    let mut claims = base_claims(&server);
    claims["aud"] = json!(["peryx-web"]);
    mount_token(
        &server,
        json!({"id_token": mint("key-1", &claims), "token_type": "Bearer"}),
    )
    .await;
    assert!(provider.callback(&response(), &pending(), NOW).await.is_ok());
}

#[tokio::test]
async fn test_missing_subject_claim_is_rejected() {
    let server = MockServer::start().await;
    mount_metadata(&server, json!({"keys": [jwk("key-1")]})).await;
    let mut raw = settings(&server.uri());
    raw.subject_claim = "employee_id".to_owned();
    let provider = OidcLoginProvider::build(raw, true).unwrap();
    mount_token(
        &server,
        json!({"id_token": mint("key-1", &base_claims(&server)), "token_type": "Bearer"}),
    )
    .await;
    assert!(matches!(
        provider.callback(&response(), &pending(), NOW).await,
        Err(OidcProviderError::InvalidClaims)
    ));
}

#[tokio::test]
async fn test_absent_display_name_falls_back_to_the_subject() {
    let (server, provider) = ready().await;
    let mut claims = base_claims(&server);
    claims.as_object_mut().unwrap().remove("name");
    mount_token(
        &server,
        json!({"id_token": mint("key-1", &claims), "token_type": "Bearer"}),
    )
    .await;
    let login = provider.callback(&response(), &pending(), NOW).await.unwrap();
    assert_eq!(login.display_name.display(), "subject-123");
}

#[tokio::test]
async fn test_blank_display_name_is_rejected() {
    let (server, provider) = ready().await;
    let mut claims = base_claims(&server);
    claims["name"] = "   ".into();
    mount_token(
        &server,
        json!({"id_token": mint("key-1", &claims), "token_type": "Bearer"}),
    )
    .await;
    assert!(matches!(
        provider.callback(&response(), &pending(), NOW).await,
        Err(OidcProviderError::InvalidClaims)
    ));
}

#[tokio::test]
async fn test_single_string_group_is_accepted() {
    let (server, provider) = ready().await;
    let mut claims = base_claims(&server);
    claims["groups"] = "solo".into();
    mount_token(
        &server,
        json!({"id_token": mint("key-1", &claims), "token_type": "Bearer"}),
    )
    .await;
    let login = provider.callback(&response(), &pending(), NOW).await.unwrap();
    assert_eq!(
        login.groups.iter().map(ExternalGroup::as_str).collect::<Vec<_>>(),
        vec!["solo"]
    );
}

#[tokio::test]
async fn test_absent_group_claim_asserts_no_groups() {
    let server = MockServer::start().await;
    mount_metadata(&server, json!({"keys": [jwk("key-1")]})).await;
    let mut raw = settings(&server.uri());
    raw.groups_claim = None;
    let provider = OidcLoginProvider::build(raw, true).unwrap();
    mount_token(
        &server,
        json!({"id_token": mint("key-1", &base_claims(&server)), "token_type": "Bearer"}),
    )
    .await;
    let login = provider.callback(&response(), &pending(), NOW).await.unwrap();
    assert!(login.groups.is_empty());
}

#[rstest]
#[case::wrong_type(json!(5))]
#[case::control_character(json!(["ok", "b\u{0001}d"]))]
#[tokio::test]
async fn test_invalid_group_claim_is_rejected(#[case] groups: Value) {
    let server = MockServer::start().await;
    mount_metadata(&server, json!({"keys": [jwk("key-1")]})).await;
    let provider = provider(&server.uri());
    let mut claims = base_claims(&server);
    claims["groups"] = groups;
    mount_token(
        &server,
        json!({"id_token": mint("key-1", &claims), "token_type": "Bearer"}),
    )
    .await;
    assert!(matches!(
        provider.callback(&response(), &pending(), NOW).await,
        Err(OidcProviderError::InvalidClaims)
    ));
}

#[tokio::test]
async fn test_token_exchange_error_status_fails_closed() {
    let (server, provider) = ready().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400))
        .mount(&server)
        .await;
    assert!(matches!(
        provider.callback(&response(), &pending(), NOW).await,
        Err(OidcProviderError::TokenExchange)
    ));
}

#[tokio::test]
async fn test_token_response_without_an_id_token_is_rejected() {
    let (server, provider) = ready().await;
    mount_token(&server, json!({"token_type": "Bearer"})).await;
    assert!(matches!(
        provider.callback(&response(), &pending(), NOW).await,
        Err(OidcProviderError::TokenExchange)
    ));
}

#[tokio::test]
async fn test_token_exchange_transport_failure_stays_unavailable() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = listener.local_addr().unwrap();
    drop(listener);
    let server = MockServer::start().await;
    mount_discovery(
        &server,
        json!({
            "issuer": issuer(&server),
            "authorization_endpoint": format!("{}/authorize", server.uri()),
            "token_endpoint": format!("http://{dead}/token"),
            "jwks_uri": format!("{}/jwks", server.uri()),
            "id_token_signing_alg_values_supported": ["RS256"],
        }),
        "application/json",
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/jwks"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({"keys": [jwk("key-1")]})),
        )
        .mount(&server)
        .await;
    assert!(matches!(
        provider(&server.uri()).callback(&response(), &pending(), NOW).await,
        Err(OidcProviderError::Unavailable)
    ));
}

#[tokio::test]
async fn test_discovery_pins_the_issuer() {
    let server = MockServer::start().await;
    mount_discovery(
        &server,
        json!({
            "issuer": "https://other.example",
            "authorization_endpoint": format!("{}/authorize", server.uri()),
            "token_endpoint": format!("{}/token", server.uri()),
            "jwks_uri": format!("{}/jwks", server.uri()),
            "id_token_signing_alg_values_supported": ["RS256"],
        }),
        "application/json",
    )
    .await;
    let provider = provider(&server.uri());
    assert!(matches!(
        provider.authorization(NOW).await,
        Err(OidcProviderError::InvalidProviderResponse)
    ));
}

#[tokio::test]
async fn test_discovery_requires_rs256_support() {
    let server = MockServer::start().await;
    mount_discovery(
        &server,
        json!({
            "issuer": issuer(&server),
            "authorization_endpoint": format!("{}/authorize", server.uri()),
            "token_endpoint": format!("{}/token", server.uri()),
            "jwks_uri": format!("{}/jwks", server.uri()),
            "id_token_signing_alg_values_supported": ["ES256"],
        }),
        "application/json",
    )
    .await;
    assert!(matches!(
        provider(&server.uri()).authorization(NOW).await,
        Err(OidcProviderError::InvalidProviderResponse)
    ));
}

#[tokio::test]
async fn test_invalid_endpoint_url_is_rejected() {
    let server = MockServer::start().await;
    mount_discovery(
        &server,
        json!({
            "issuer": issuer(&server),
            "authorization_endpoint": "ftp://insecure.example/authorize",
            "token_endpoint": format!("{}/token", server.uri()),
            "jwks_uri": format!("{}/jwks", server.uri()),
            "id_token_signing_alg_values_supported": ["RS256"],
        }),
        "application/json",
    )
    .await;
    assert!(matches!(
        provider(&server.uri()).authorization(NOW).await,
        Err(OidcProviderError::InvalidProviderResponse)
    ));
}

#[tokio::test]
async fn test_non_json_discovery_is_rejected() {
    let server = MockServer::start().await;
    mount_discovery(&server, json!({"issuer": server.uri()}), "text/plain").await;
    assert!(matches!(
        provider(&server.uri()).authorization(NOW).await,
        Err(OidcProviderError::InvalidProviderResponse)
    ));
}

#[tokio::test]
async fn test_oversize_discovery_is_rejected() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("x".repeat(DISCOVERY_BODY_LIMIT + 1), "application/json"))
        .mount(&server)
        .await;
    assert!(matches!(
        provider(&server.uri()).authorization(NOW).await,
        Err(OidcProviderError::InvalidProviderResponse)
    ));
}

#[tokio::test]
async fn test_cold_provider_failure_is_rate_limited() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let provider = provider(&server.uri());
    assert!(matches!(provider.authorization(NOW).await, Err(error) if error.unavailable()));
    assert!(matches!(provider.authorization(NOW).await, Err(error) if error.unavailable()));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_cold_provider_network_failure_is_unavailable() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    assert!(matches!(
        provider(&format!("http://{address}")).authorization(NOW).await,
        Err(OidcProviderError::Unavailable)
    ));
}

#[tokio::test]
async fn test_signing_key_rotation_succeeds_after_refresh() {
    let (server, provider) = ready().await;
    mount_token(
        &server,
        json!({"id_token": mint("key-1", &base_claims(&server)), "token_type": "Bearer"}),
    )
    .await;
    provider.callback(&response(), &pending(), NOW).await.unwrap();
    server.reset().await;
    mount_metadata(&server, json!({"keys": [jwk("key-2")]})).await;
    mount_token(
        &server,
        json!({"id_token": mint("key-2", &base_claims(&server)), "token_type": "Bearer"}),
    )
    .await;
    assert!(
        provider
            .callback(&response(), &pending(), NOW + MAX_FRESH_SECS)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn test_unknown_key_refresh_is_rate_limited() {
    let (server, provider) = ready().await;
    mount_token(
        &server,
        json!({"id_token": mint("key-1", &base_claims(&server)), "token_type": "Bearer"}),
    )
    .await;
    provider.callback(&response(), &pending(), NOW).await.unwrap();
    server.reset().await;
    mount_metadata(&server, json!({"keys": [jwk("key-1")]})).await;
    mount_token(
        &server,
        json!({"id_token": mint("key-2", &base_claims(&server)), "token_type": "Bearer"}),
    )
    .await;
    assert!(matches!(
        provider.callback(&response(), &pending(), NOW).await,
        Err(OidcProviderError::UnknownKey)
    ));
    assert!(matches!(
        provider.callback(&response(), &pending(), NOW + MIN_FRESH_SECS).await,
        Err(OidcProviderError::UnknownKey)
    ));
}

#[tokio::test]
async fn test_metadata_outage_keeps_the_cached_key_then_hard_expires() {
    let (server, provider) = ready().await;
    mount_token(
        &server,
        json!({"id_token": mint("key-1", &base_claims(&server)), "token_type": "Bearer"}),
    )
    .await;
    provider.callback(&response(), &pending(), NOW).await.unwrap();
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    mount_token(
        &server,
        json!({"id_token": mint("key-1", &base_claims(&server)), "token_type": "Bearer"}),
    )
    .await;
    assert!(
        provider
            .callback(&response(), &pending(), NOW + MAX_FRESH_SECS)
            .await
            .is_ok()
    );
    assert!(matches!(
        provider.callback(&response(), &pending(), NOW + HARD_CACHE_SECS + 1).await,
        Err(error) if error.unavailable()
    ));
}

#[tokio::test]
async fn test_warm_cache_serves_without_refetching() {
    let (server, provider) = ready().await;
    mount_token(
        &server,
        json!({"id_token": mint("key-1", &base_claims(&server)), "token_type": "Bearer"}),
    )
    .await;
    provider.callback(&response(), &pending(), NOW).await.unwrap();
    provider.callback(&response(), &pending(), NOW + 1).await.unwrap();
    let discovery = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/.well-known/openid-configuration")
        .count();
    assert_eq!(discovery, 1);
}

#[rstest]
#[case::empty(json!({"keys": []}))]
#[case::duplicate(json!({"keys": [jwk("dup"), jwk("dup")]}))]
#[case::no_kid(json!({"keys": [{"kty": "RSA", "n": MODULUS, "e": "AQAB", "alg": "RS256"}]}))]
#[case::no_usable(json!({"keys": [{"kty": "oct", "k": "c2VjcmV0", "kid": "sym", "alg": "HS256"}]}))]
#[case::bad_modulus(json!({"keys": [{"kty": "RSA", "n": "!", "e": "AQAB", "kid": "key-1", "alg": "RS256"}]}))]
#[tokio::test]
async fn test_unusable_key_sets_are_rejected(#[case] keys: Value) {
    let server = MockServer::start().await;
    mount_metadata(&server, keys).await;
    assert!(matches!(
        provider(&server.uri()).authorization(NOW).await,
        Err(OidcProviderError::InvalidProviderResponse)
    ));
}

#[tokio::test]
async fn test_verify_only_key_survives_alongside_incompatible_keys() {
    let server = MockServer::start().await;
    mount_metadata(
        &server,
        json!({"keys": [
            {"kty": "oct", "k": "c2VjcmV0", "kid": "sym", "alg": "HS256"},
            {"kty": "RSA", "n": MODULUS, "e": "AQAB", "kid": "sign-only", "alg": "RS256", "use": "sig", "key_ops": ["sign"]},
            {"kty": "RSA", "n": MODULUS, "e": "AQAB", "kid": "key-1", "alg": "RS256", "use": "sig", "key_ops": ["verify"]}
        ]}),
    )
    .await;
    let provider = provider(&server.uri());
    mount_token(
        &server,
        json!({"id_token": mint("key-1", &base_claims(&server)), "token_type": "Bearer"}),
    )
    .await;
    assert!(provider.callback(&response(), &pending(), NOW).await.is_ok());
}

#[tokio::test]
async fn test_chunked_discovery_body_is_bounded_while_streaming() {
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
    assert!(matches!(
        provider(&format!("http://{address}")).authorization(NOW).await,
        Err(OidcProviderError::InvalidProviderResponse)
    ));
}

#[tokio::test]
async fn test_truncated_discovery_body_is_unavailable() {
    use std::io::{Read as _, Write as _};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut request = [0; 1024];
        let _ = socket.read(&mut request);
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 8\r\nconnection: close\r\n\r\n{}")
            .unwrap();
    });
    assert!(matches!(
        provider(&format!("http://{address}")).authorization(NOW).await,
        Err(OidcProviderError::Unavailable)
    ));
}

#[rstest]
#[case::json("application/json; charset=utf-8", true)]
#[case::structured("application/jwk-set+json", true)]
#[case::wrong("text/json", false)]
fn test_json_content_type_classification(#[case] value: &str, #[case] accepted: bool) {
    assert_eq!(is_json_content_type(value), accepted);
}

#[rstest]
#[case::present("max-age=42", Some(42))]
#[case::quoted("private, max-age=\"7\"", Some(7))]
#[case::absent("no-store", None)]
fn test_cache_max_age_parsing(#[case] value: &str, #[case] expected: Option<i64>) {
    assert_eq!(cache_max_age(value), expected);
}

#[test]
fn test_scope_string_inserts_and_deduplicates_openid() {
    assert_eq!(scope_string(&[]), "openid");
    assert_eq!(
        scope_string(&["openid".to_owned(), "email".to_owned(), "email".to_owned()]),
        "openid email"
    );
}

#[rstest]
#[case::http_issuer(OidcProviderBuildError::InvalidIssuer, |s: &mut OidcProviderSettings| s.issuer = Url::parse("http://issuer.example").unwrap())]
#[case::issuer_query(OidcProviderBuildError::InvalidIssuer, |s: &mut OidcProviderSettings| s.issuer = Url::parse("https://issuer.example/?x=1").unwrap())]
#[case::redirect_fragment(OidcProviderBuildError::InvalidRedirectUri, |s: &mut OidcProviderSettings| s.redirect_uri = Url::parse("https://app.example/cb#x").unwrap())]
#[case::empty_client(OidcProviderBuildError::EmptyClientId, |s: &mut OidcProviderSettings| s.client_id.clear())]
#[case::empty_subject(OidcProviderBuildError::InvalidClaim, |s: &mut OidcProviderSettings| s.subject_claim.clear())]
#[case::empty_display(OidcProviderBuildError::InvalidClaim, |s: &mut OidcProviderSettings| s.display_name_claim.clear())]
#[case::empty_group(OidcProviderBuildError::InvalidClaim, |s: &mut OidcProviderSettings| s.groups_claim = Some(String::new()))]
#[case::zero_timeout(OidcProviderBuildError::InvalidTimeout, |s: &mut OidcProviderSettings| s.request_timeout = Duration::ZERO)]
fn test_new_rejects_invalid_settings(
    #[case] expected: OidcProviderBuildError,
    #[case] mutate: fn(&mut OidcProviderSettings),
) {
    let mut raw = settings("https://issuer.example");
    mutate(&mut raw);
    assert_eq!(OidcLoginProvider::new(raw).unwrap_err(), expected);
}

#[test]
fn test_new_accepts_a_secure_provider() {
    let provider = OidcLoginProvider::new(settings("https://issuer.example")).unwrap();
    assert_eq!(provider.id().as_str(), "corporate");
}

#[rstest]
#[case::unavailable(OidcProviderError::Unavailable, true)]
#[case::invalid_response(OidcProviderError::InvalidProviderResponse, true)]
#[case::unknown_key(OidcProviderError::UnknownKey, true)]
#[case::state(OidcProviderError::StateMismatch, false)]
#[case::exchange(OidcProviderError::TokenExchange, false)]
#[case::token(OidcProviderError::InvalidToken, false)]
#[case::claims(OidcProviderError::InvalidClaims, false)]
fn test_error_availability(#[case] error: OidcProviderError, #[case] expected: bool) {
    assert_eq!(error.unavailable(), expected);
}

// One concrete store keeps the suite to a single OidcLoginService monomorphization; per-test
// closure stores would leave each mono's uncalled methods counted as uncovered on x86.
#[derive(Clone)]
struct TestStore {
    outcome: Result<ExternalIdentityResolution, &'static str>,
    calls: Arc<Mutex<usize>>,
}

impl TestStore {
    fn ok() -> Self {
        Self {
            outcome: Ok(resolution()),
            calls: Arc::new(Mutex::new(0)),
        }
    }

    fn failing() -> Self {
        Self {
            outcome: Err("store down"),
            calls: Arc::new(Mutex::new(0)),
        }
    }

    fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

impl crate::ExternalIdentityStore for TestStore {
    type Error = &'static str;

    fn link_or_resolve(&self, _request: ExternalLinkRequest) -> Result<ExternalIdentityResolution, Self::Error> {
        *self.calls.lock().unwrap() += 1;
        self.outcome.clone()
    }
}

fn resolution() -> ExternalIdentityResolution {
    ExternalIdentityResolution {
        user: ServerUser {
            id: UserId::random(),
            name: UserName::new("Ada").unwrap(),
            state: UserState::Active,
            revision: 1,
        },
        link_created: true,
        grants_changed: false,
    }
}

fn service(issuer: &str, store: TestStore) -> OidcLoginService<TestStore> {
    OidcLoginService::new(provider(issuer), store, Vec::new())
}

#[test]
fn test_debug_redacts_secrets() {
    let provider = provider("https://issuer.example");
    let rendered = format!("{provider:?}");
    assert!(rendered.contains("[redacted]"));
    assert!(!rendered.contains("s3cret"));
    assert_eq!(format!("{:?}", pending()), "PendingLogin([redacted])");
    let service = OidcLoginService::new(provider, TestStore::ok(), Vec::new());
    assert!(format!("{service:?}").contains("OidcLoginService"));
}

#[tokio::test]
async fn test_service_authorizes_and_commits_the_link() {
    let server = MockServer::start().await;
    mount_metadata(&server, json!({"keys": [jwk("key-1")]})).await;
    mount_token(
        &server,
        json!({"id_token": mint("key-1", &base_claims(&server)), "token_type": "Bearer"}),
    )
    .await;
    let store = TestStore::ok();
    let service = service(&server.uri(), store.clone());
    assert_eq!(service.id().as_str(), "corporate");
    assert!(service.authorization(NOW).await.is_ok());
    let outcome = service.callback(&response(), &pending(), NOW).await.unwrap();
    assert!(outcome.link_created);
    assert_eq!(store.calls(), 1);
}

#[tokio::test]
async fn test_service_reports_a_store_failure() {
    let server = MockServer::start().await;
    mount_metadata(&server, json!({"keys": [jwk("key-1")]})).await;
    mount_token(
        &server,
        json!({"id_token": mint("key-1", &base_claims(&server)), "token_type": "Bearer"}),
    )
    .await;
    let store = TestStore::failing();
    let service = service(&server.uri(), store.clone());
    assert!(matches!(
        service.callback(&response(), &pending(), NOW).await,
        Err(OidcLoginError::Store("store down"))
    ));
    assert_eq!(store.calls(), 1);
}

#[tokio::test]
async fn test_service_propagates_a_provider_failure() {
    let server = MockServer::start().await;
    mount_metadata(&server, json!({"keys": [jwk("key-1")]})).await;
    mount_token(&server, json!({"token_type": "Bearer"})).await;
    let store = TestStore::ok();
    let service = service(&server.uri(), store.clone());
    assert!(matches!(
        service.callback(&response(), &pending(), NOW).await,
        Err(OidcLoginError::Provider(OidcProviderError::TokenExchange))
    ));
    assert_eq!(store.calls(), 0);
}
