use std::sync::Mutex;

use jsonwebtoken::{EncodingKey, Header};
use rstest::rstest;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::tests::oidc_http::{
    MAX_DISCOVERY_BYTES, TestHttpServer, TestResponseBody, insecure_transport, padded_json, secure_origin, transport,
};
use crate::{ExternalIdentityResolution, ExternalLinkRequest, ServerUser, UserId, UserState};

const NOW: i64 = 2_000_000_000;
const FAILED_REFRESH_BACKOFF_SECS: i64 = 60;
const METADATA_FRESH_SECS: i64 = 120;
const MAX_METADATA_FRESH_SECS: i64 = 900;
const HARD_CACHE_SECS: i64 = 3_600;
const MAX_TOKEN_RESPONSE_BYTES: usize = 65_536;
const MODULUS: &str = "yRE6rHuNR0QbHO3H3Kt2pOKGVhQqGZXInOduQNxXzuKlvQTLUTv4l4sggh5_CYYi_cvI-SXVT9kPWSKXxJXBXd_4LkvcPuUakBoAkfh-eiFVMh2VrUyWyj3MFl0HTVF9KwRXLAcwkREiS3npThHRyIxuy0ZMeZfxVL5arMhw1SRELB8HoGfG_AtH89BIE9jDBHZ9dLelK9a184zAf8LwoPLxvJb3Il5nncqPcSfKDDodMFBIMc4lQzDKL5gvmiXLXB1AGLm8KBjfE8s3L5xqi-yUod-j8MtvIj812dkS4QMiRVN_by2h3ZY8LYVGrqZXZTcgn2ujn8uKjXLZVD5TdQ";
const PRIVATE_KEY_DER: &str = "MIIEpAIBAAKCAQEAyRE6rHuNR0QbHO3H3Kt2pOKGVhQqGZXInOduQNxXzuKlvQTLUTv4l4sggh5/CYYi/cvI+SXVT9kPWSKXxJXBXd/4LkvcPuUakBoAkfh+eiFVMh2VrUyWyj3MFl0HTVF9KwRXLAcwkREiS3npThHRyIxuy0ZMeZfxVL5arMhw1SRELB8HoGfG/AtH89BIE9jDBHZ9dLelK9a184zAf8LwoPLxvJb3Il5nncqPcSfKDDodMFBIMc4lQzDKL5gvmiXLXB1AGLm8KBjfE8s3L5xqi+yUod+j8MtvIj812dkS4QMiRVN/by2h3ZY8LYVGrqZXZTcgn2ujn8uKjXLZVD5TdQIDAQABAoIBAHREk0I0O9DvECKdWUpAmF3mY7oY9PNQiu44Yaf+AoSuyRpRUGTMIgc3u3eivOE8ALX0BmYUO5JtuRNZDpvt4SAwqCnVUinIf6C+eH/wSurCpapSM0BAHp4aOA7igptyOMgMPYBHNA1e9A7jE0dCxKWMl3DSWNyjQTk4zeRGEAEfbNjHrq6YCtjHSZSLmWiG80hnfnYos9hOr5JnLnyS7ZmFE/5P3XVrxLc/tQ5zum0R4cbrgzHiQP5RgfxGJaEi7XcgherCCOgurJSSbYH29Gz8u5fFbS+Yg8s+OiCss3cs1rSgJ9/eHZuzGEdUZVARH6hVMjSuwvqVTFaE8AgtleECgYEA+uLMn4kNqHlJS2A5uAnCkj90ZxEtNm3E8hAxUrhssktY5XSOAPBlxyf5RuRGIImGtUVIr4HuJSa5TX48n3Vdt9MYCprO/iYl6moNRSPt5qowIIOJmIjY2mqPDfDt/zw+fcDD3lmCJrFlzcnh0uea1CohxEbQnL3cypeLt+WbU6kCgYEAzSp19m1ajieFkqgoB0YTpt/OroDx38vvI5unInJlEeOjQ+oIAQdN2wpxBvTrRorMU6P07mFUbt1j+Co6CbNiw+X8HcCaqYLR5clbJOOWNR36PuzOpQLkfK8woupBxzW9B8gZmY8rB1mbJ+/WTPrEJy6YGmIEBkWylQ2VpW8O4O0CgYEApdbvvfFBlwD9YxbrcGz7MeNCFbMz+MucqQntIKoKJ91ImPxvtc0y6e/Rhnv0oyNlaUOwJVu0yNgNG117w0g4t/+Q38mvVC5xV7/cn7x9UMFk6MkqVir3dYGEqIl/OP1grY2Tq9HtB5iyG9L8NIamQOLMyUqqMUILxdthHyFmiGkCgYEAn9+PjpjGMPHxL0gj8Q8VbzsFtou6b1deIRRA2CHmSltltR1gYVTMwXxQeUhPMmgkMqUXzs4/WijgpthY44hK1TaZEKIuoxrS70nJ4WQLf5a9k1065fDsFZD6yGjdGxvwEmlGMZgTwqV7t1I4X0Ilqhav5hcs5apYL7gnPYPeRz0CgYALHCj/Ji8XSsDoF/MhVhnGdIs2P99NNdmo3R2Pv0CuZbDKMU559LJHUvrKS8WkuWRDuKrz1W/EQKApFjDGpdqToZqriUFQzwy7mR3ayIiogzNtHcvbDHx8oFnGY0OFksX/ye0/XGpy2SFxYRwGU98HPYeBvAQQrVjdkzfy7BmXQQ==";

fn settings(issuer: &str) -> OidcProviderSettings {
    OidcProviderSettings {
        id: ProviderId::new("corporate").unwrap(),
        issuer: issuer.to_owned(),
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
    provider_with_settings(settings(&secure_origin(issuer)), issuer)
}

fn provider_with_settings(settings: OidcProviderSettings, destination: &str) -> OidcLoginProvider {
    OidcLoginProvider::with_http_transport(settings, transport(destination)).unwrap()
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
    secure_origin(&server.uri())
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
            "authorization_endpoint": format!("{}/authorize", secure_origin(&server.uri())),
            "token_endpoint": format!("{}/token", secure_origin(&server.uri())),
            "jwks_uri": format!("{}/jwks", secure_origin(&server.uri())),
            "id_token_signing_alg_values_supported": ["RS256"],
        }),
        "application/json",
    )
    .await;
    mount_jwks(server, keys).await;
}

async fn mount_jwks(server: &MockServer, keys: Value) {
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

async fn discovery_requests(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/.well-known/openid-configuration")
        .count()
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
#[case::wrong_issuer("iss", Some(json!("https://evil.example")), false)]
#[case::wrong_audience("aud", Some(json!("other-client")), false)]
#[case::expiration_before_boundary("exp", Some(json!(NOW - 59)), true)]
#[case::expiration_at_boundary("exp", Some(json!(NOW - 60)), false)]
#[case::expiration_beyond_boundary("exp", Some(json!(NOW - 61)), false)]
#[case::expiration_maximum("exp", Some(json!(i64::MAX)), true)]
#[case::issued_at_skew("iat", Some(json!(NOW + 60)), true)]
#[case::issued_beyond_skew("iat", Some(json!(NOW + 61)), false)]
#[case::not_before_absent("nbf", None, true)]
#[case::not_before_at_skew("nbf", Some(json!(NOW + 60)), true)]
#[case::not_before_beyond_skew("nbf", Some(json!(NOW + 61)), false)]
#[case::not_before_minimum("nbf", Some(json!(i64::MIN)), true)]
#[case::not_before_nonnumeric("nbf", Some(json!("later")), false)]
#[tokio::test]
async fn test_id_token_registered_claims_are_enforced(
    #[case] claim: &str,
    #[case] value: Option<Value>,
    #[case] accepted: bool,
) {
    let server = MockServer::start().await;
    mount_metadata(&server, json!({"keys": [jwk("key-1")]})).await;
    let mut claims = base_claims(&server);
    if let Some(value) = value {
        claims[claim] = value;
    }
    mount_token(
        &server,
        json!({"id_token": mint("key-1", &claims), "token_type": "Bearer"}),
    )
    .await;
    let store = TestStore::ok();
    assert_eq!(
        (
            service(&server.uri(), store.clone())
                .callback(&response(), &pending(), NOW)
                .await
                .is_ok(),
            store.calls(),
        ),
        (accepted, usize::from(accepted))
    );
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

#[rstest]
#[case::matching_authorized_party(json!("peryx-web"), Some("peryx-web"))]
#[case::multiple_audiences(json!(["peryx-web", "other-client"]), Some("peryx-web"))]
#[case::single_audience_array(json!(["peryx-web"]), None)]
#[tokio::test]
async fn test_accepted_audience_returns_identity_and_claims(
    #[case] audience: Value,
    #[case] authorized_party: Option<&str>,
) {
    let (server, provider) = ready().await;
    let mut claims = base_claims(&server);
    claims["aud"] = audience;
    if let Some(authorized_party) = authorized_party {
        claims["azp"] = authorized_party.into();
    }
    mount_token(
        &server,
        json!({"id_token": mint("key-1", &claims), "token_type": "Bearer"}),
    )
    .await;
    let login = provider.callback(&response(), &pending(), NOW).await.unwrap();
    assert_eq!(login.identity.subject.as_str(), "subject-123");
    assert_eq!(login.identity.provider.as_str(), "corporate");
    assert_eq!(login.display_name.display(), "Ada Lovelace");
    assert_eq!(
        login.groups.iter().map(ExternalGroup::as_str).collect::<Vec<_>>(),
        vec!["dev", "ops"]
    );
}

#[tokio::test]
async fn test_missing_subject_claim_is_rejected() {
    let server = MockServer::start().await;
    mount_metadata(&server, json!({"keys": [jwk("key-1")]})).await;
    let mut raw = settings(&secure_origin(&server.uri()));
    raw.subject_claim = "employee_id".to_owned();
    let provider = provider_with_settings(raw, &server.uri());
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

#[rstest]
#[case::single(json!("solo"), vec!["solo"])]
#[case::sorted_and_deduplicated(json!(["ops", "dev", "ops"]), vec!["dev", "ops"])]
#[tokio::test]
async fn test_valid_group_claim_is_normalized(#[case] groups: Value, #[case] expected: Vec<&str>) {
    let (server, provider) = ready().await;
    let mut claims = base_claims(&server);
    claims["groups"] = groups;
    mount_token(
        &server,
        json!({"id_token": mint("key-1", &claims), "token_type": "Bearer"}),
    )
    .await;
    let login = provider.callback(&response(), &pending(), NOW).await.unwrap();
    assert_eq!(
        login.groups.iter().map(ExternalGroup::as_str).collect::<Vec<_>>(),
        expected
    );
}

#[tokio::test]
async fn test_absent_group_claim_asserts_no_groups() {
    let server = MockServer::start().await;
    mount_metadata(&server, json!({"keys": [jwk("key-1")]})).await;
    let mut raw = settings(&secure_origin(&server.uri()));
    raw.groups_claim = None;
    let provider = provider_with_settings(raw, &server.uri());
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
#[case::integer_array(json!([42]))]
#[case::mixed_array(json!(["release-admin", {"name": "developers"}]))]
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

#[rstest]
#[case::invalid_request(
    400,
    "application/json",
    r#"{"error":"invalid_request"}"#,
    OidcTokenExchangeError::Protocol { status: 400, code: OidcTokenErrorCode::InvalidRequest }
)]
#[case::invalid_client(
    401,
    "application/json",
    r#"{"error":"invalid_client"}"#,
    OidcTokenExchangeError::Protocol { status: 401, code: OidcTokenErrorCode::InvalidClient }
)]
#[case::invalid_grant(
    400,
    "application/json",
    concat!(
        r#"{"error":"invalid_grant","error_description":"provider-secret","#,
        r#""access_token":"token-secret","error_uri":"https://provider.example/secret"}"#
    ),
    OidcTokenExchangeError::Protocol { status: 400, code: OidcTokenErrorCode::InvalidGrant }
)]
#[case::unauthorized_client(
    400,
    "application/json",
    r#"{"error":"unauthorized_client"}"#,
    OidcTokenExchangeError::Protocol { status: 400, code: OidcTokenErrorCode::UnauthorizedClient }
)]
#[case::unsupported_grant_type(
    400,
    "application/json",
    r#"{"error":"unsupported_grant_type"}"#,
    OidcTokenExchangeError::Protocol { status: 400, code: OidcTokenErrorCode::UnsupportedGrantType }
)]
#[case::invalid_scope(
    400,
    "application/json",
    r#"{"error":"invalid_scope"}"#,
    OidcTokenExchangeError::Protocol { status: 400, code: OidcTokenErrorCode::InvalidScope }
)]
#[case::unknown(
    400,
    "application/json",
    r#"{"error":"provider_extension"}"#,
    OidcTokenExchangeError::Protocol { status: 400, code: OidcTokenErrorCode::Unknown }
)]
#[case::server_invalid_grant(
    500,
    "application/json",
    r#"{"error":"invalid_grant"}"#,
    OidcTokenExchangeError::Protocol { status: 500, code: OidcTokenErrorCode::InvalidGrant }
)]
#[case::success_invalid_grant(
    200,
    "application/json",
    r#"{"error":"invalid_grant"}"#,
    OidcTokenExchangeError::InvalidResponse { status: 200 }
)]
#[case::wrong_content_type(
    400,
    "text/plain",
    r#"{"error":"invalid_grant"}"#,
    OidcTokenExchangeError::InvalidResponse { status: 400 }
)]
#[case::html(500, "text/html", "<h1>provider-secret</h1>", OidcTokenExchangeError::InvalidResponse { status: 500 })]
#[case::malformed(400, "application/json", r#"{"error":"invalid_grant"#,
    OidcTokenExchangeError::InvalidResponse { status: 400 })]
#[case::non_string_error(
    400,
    "application/json",
    r#"{"error":5}"#,
    OidcTokenExchangeError::InvalidResponse { status: 400 }
)]
#[case::missing_id_token(
    200,
    "application/json",
    r#"{"token_type":"Bearer"}"#,
    OidcTokenExchangeError::InvalidResponse { status: 200 }
)]
#[tokio::test]
async fn test_token_exchange_failure_is_typed_and_redacted(
    #[case] status: u16,
    #[case] content_type: &str,
    #[case] body: &str,
    #[case] expected: OidcTokenExchangeError,
) {
    let (server, provider) = ready().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(status).set_body_raw(body, content_type))
        .mount(&server)
        .await;
    let error = provider.callback(&response(), &pending(), NOW).await.unwrap_err();
    assert_eq!(error, OidcProviderError::TokenExchange(expected));
    let rendered = format!("{error:?} {error}");
    let provider_url = server.uri();
    assert!(
        ![
            "provider-secret",
            "token-secret",
            "provider.example",
            provider_url.as_str()
        ]
        .into_iter()
        .any(|secret| rendered.contains(secret))
    );
}

#[rstest]
#[case::maximum(MAX_TOKEN_RESPONSE_BYTES, true)]
#[case::oversized(MAX_TOKEN_RESPONSE_BYTES + 1, false)]
#[tokio::test]
async fn test_token_response_size_bound(#[case] size: usize, #[case] accepted: bool) {
    let (server, provider) = ready().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            padded_json(
                json!({"id_token": mint("key-1", &base_claims(&server)), "token_type": "Bearer"}),
                size,
            ),
            "application/json",
        ))
        .mount(&server)
        .await;

    let result = provider.callback(&response(), &pending(), NOW).await;
    if accepted {
        assert!(result.is_ok());
    } else {
        assert_eq!(
            result.unwrap_err(),
            OidcProviderError::TokenExchange(OidcTokenExchangeError::InvalidResponse { status: 200 })
        );
    }
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
            "authorization_endpoint": format!("{}/authorize", secure_origin(&server.uri())),
            "token_endpoint": format!("https://{dead}/token"),
            "jwks_uri": format!("{}/jwks", secure_origin(&server.uri())),
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
        Err(OidcProviderError::TokenExchange(OidcTokenExchangeError::Transport {
            status: None
        }))
    ));
}

#[tokio::test]
async fn test_token_body_read_failure_retains_the_http_status() {
    let token_server = TestHttpServer::start(TestResponseBody::Truncated);
    let server = MockServer::start().await;
    mount_discovery(
        &server,
        json!({
            "issuer": issuer(&server),
            "authorization_endpoint": format!("{}/authorize", secure_origin(&server.uri())),
            "token_endpoint": format!("{}/token", secure_origin(&token_server.origin())),
            "jwks_uri": format!("{}/jwks", secure_origin(&server.uri())),
            "id_token_signing_alg_values_supported": ["RS256"],
        }),
        "application/json",
    )
    .await;
    mount_jwks(&server, json!({"keys": [jwk("key-1")]})).await;
    let provider = OidcLoginProvider::with_http_transport(settings(&issuer(&server)), insecure_transport()).unwrap();

    assert_eq!(
        provider.callback(&response(), &pending(), NOW).await.unwrap_err(),
        OidcProviderError::TokenExchange(OidcTokenExchangeError::Transport { status: Some(200) })
    );
}

#[tokio::test]
async fn test_discovery_pins_the_issuer() {
    let server = MockServer::start().await;
    mount_discovery(
        &server,
        json!({
            "issuer": "https://other.example",
            "authorization_endpoint": format!("{}/authorize", secure_origin(&server.uri())),
            "token_endpoint": format!("{}/token", secure_origin(&server.uri())),
            "jwks_uri": format!("{}/jwks", secure_origin(&server.uri())),
            "id_token_signing_alg_values_supported": ["RS256"],
        }),
        "application/json",
    )
    .await;
    mount_jwks(&server, json!({"keys": [jwk("key-1")]})).await;
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
            "authorization_endpoint": format!("{}/authorize", secure_origin(&server.uri())),
            "token_endpoint": format!("{}/token", secure_origin(&server.uri())),
            "jwks_uri": format!("{}/jwks", secure_origin(&server.uri())),
            "id_token_signing_alg_values_supported": ["ES256"],
        }),
        "application/json",
    )
    .await;
    mount_jwks(&server, json!({"keys": [jwk("key-1")]})).await;
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
            "token_endpoint": format!("{}/token", secure_origin(&server.uri())),
            "jwks_uri": format!("{}/jwks", secure_origin(&server.uri())),
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

#[rstest]
#[case::json("application/json; charset=utf-8", true)]
#[case::structured("application/openid-configuration+json", true)]
#[case::wrong("text/json", false)]
#[tokio::test]
async fn test_discovery_content_type_is_enforced(#[case] content_type: &str, #[case] accepted: bool) {
    let server = MockServer::start().await;
    mount_discovery(
        &server,
        json!({
            "issuer": issuer(&server),
            "authorization_endpoint": format!("{}/authorize", secure_origin(&server.uri())),
            "token_endpoint": format!("{}/token", secure_origin(&server.uri())),
            "jwks_uri": format!("{}/jwks", secure_origin(&server.uri())),
            "id_token_signing_alg_values_supported": ["RS256"],
        }),
        content_type,
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

    assert_eq!(provider(&server.uri()).authorization(NOW).await.is_ok(), accepted);
}

#[tokio::test]
async fn test_oversize_discovery_is_rejected() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("x".repeat(MAX_DISCOVERY_BYTES + 1), "application/json"))
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
    let provider = OidcLoginProvider::new(settings(&format!("https://{address}"))).unwrap();
    assert!(matches!(
        provider.authorization(NOW).await,
        Err(OidcProviderError::Unavailable)
    ));
}

#[tokio::test]
async fn test_signing_key_rotation_refreshes_a_fresh_cache() {
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
            .callback(&response(), &pending(), NOW + FAILED_REFRESH_BACKOFF_SECS)
            .await
            .is_ok()
    );
}

#[rstest]
#[case::successful(true)]
#[case::failed(false)]
#[tokio::test]
async fn test_unknown_key_refresh_is_rate_limited(#[case] refresh_succeeds: bool) {
    let (server, provider) = ready().await;
    mount_token(
        &server,
        json!({"id_token": mint("key-1", &base_claims(&server)), "token_type": "Bearer"}),
    )
    .await;
    provider.callback(&response(), &pending(), NOW).await.unwrap();
    server.reset().await;
    if refresh_succeeds {
        mount_metadata(&server, json!({"keys": [jwk("key-1")]})).await;
    } else {
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
    }
    mount_token(
        &server,
        json!({"id_token": mint("key-2", &base_claims(&server)), "token_type": "Bearer"}),
    )
    .await;
    assert!(matches!(
        provider
            .callback(&response(), &pending(), NOW + FAILED_REFRESH_BACKOFF_SECS)
            .await,
        Err(OidcProviderError::UnknownKey)
    ));
    assert_eq!(discovery_requests(&server).await, 1);
    assert!(matches!(
        provider
            .callback(&response(), &pending(), NOW + 2 * FAILED_REFRESH_BACKOFF_SECS - 1)
            .await,
        Err(OidcProviderError::UnknownKey)
    ));
    assert_eq!(discovery_requests(&server).await, 1);
    assert!(matches!(
        provider
            .callback(&response(), &pending(), NOW + 2 * FAILED_REFRESH_BACKOFF_SECS)
            .await,
        Err(OidcProviderError::UnknownKey)
    ));
    assert_eq!(discovery_requests(&server).await, 2);
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
            .callback(&response(), &pending(), NOW + MAX_METADATA_FRESH_SECS)
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
    provider
        .callback(&response(), &pending(), NOW + METADATA_FRESH_SECS - 1)
        .await
        .unwrap();
    assert_eq!(discovery_requests(&server).await, 1);
    provider
        .callback(&response(), &pending(), NOW + METADATA_FRESH_SECS)
        .await
        .unwrap();
    assert_eq!(discovery_requests(&server).await, 2);
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
    let login = provider.callback(&response(), &pending(), NOW).await.unwrap();
    assert_eq!(
        (
            login.identity.provider.as_str(),
            login.identity.subject.as_str(),
            login.display_name.display(),
            login.groups.iter().map(ExternalGroup::as_str).collect::<Vec<_>>(),
        ),
        ("corporate", "subject-123", "Ada Lovelace", vec!["dev", "ops"])
    );
}

#[rstest]
#[case::oversized_chunk(
    TestResponseBody::OversizedChunked { limit: MAX_DISCOVERY_BYTES },
    OidcProviderError::InvalidProviderResponse
)]
#[case::truncated(TestResponseBody::Truncated, OidcProviderError::Unavailable)]
#[tokio::test]
async fn test_malformed_discovery_body(#[case] body: TestResponseBody, #[case] expected: OidcProviderError) {
    let server = TestHttpServer::start(body);
    assert_eq!(
        provider(&server.origin()).authorization(NOW).await.unwrap_err(),
        expected
    );
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
#[case::http_issuer(OidcProviderBuildError::InvalidIssuer, |s: &mut OidcProviderSettings| s.issuer = "http://issuer.example".to_owned())]
#[case::issuer_query(OidcProviderBuildError::InvalidIssuer, |s: &mut OidcProviderSettings| s.issuer = "https://issuer.example/?x=1".to_owned())]
#[case::issuer_userinfo(OidcProviderBuildError::InvalidIssuer, |s: &mut OidcProviderSettings| s.issuer = "https://user@issuer.example".to_owned())]
#[case::normalized_issuer(OidcProviderBuildError::InvalidIssuer, |s: &mut OidcProviderSettings| s.issuer = "https://ISSUER.example".to_owned())]
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
#[case::unavailable(OidcProviderError::Unavailable, true, false)]
#[case::invalid_response(OidcProviderError::InvalidProviderResponse, true, false)]
#[case::unknown_key(OidcProviderError::UnknownKey, true, false)]
#[case::state(OidcProviderError::StateMismatch, false, false)]
#[case::exchange_transport(
    OidcProviderError::TokenExchange(OidcTokenExchangeError::Transport { status: None }),
    true,
    false
)]
#[case::exchange_invalid_grant(
    OidcProviderError::TokenExchange(OidcTokenExchangeError::Protocol {
        status: 400,
        code: OidcTokenErrorCode::InvalidGrant,
    }),
    false,
    true
)]
#[case::exchange_invalid_client(
    OidcProviderError::TokenExchange(OidcTokenExchangeError::Protocol {
        status: 401,
        code: OidcTokenErrorCode::InvalidClient,
    }),
    false,
    false
)]
#[case::exchange_invalid_response(
    OidcProviderError::TokenExchange(OidcTokenExchangeError::InvalidResponse { status: 500 }),
    false,
    false
)]
#[case::token(OidcProviderError::InvalidToken, false, false)]
#[case::claims(OidcProviderError::InvalidClaims, false, false)]
fn test_error_classification(
    #[case] error: OidcProviderError,
    #[case] unavailable: bool,
    #[case] authentication_rejected: bool,
) {
    assert_eq!(
        (error.unavailable(), error.authentication_rejected()),
        (unavailable, authentication_rejected)
    );
}

// One store avoids uncovered methods from per-test monomorphizations on x86.
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
    let expected = format!("OidcLoginService {{ provider: {provider:?}, group_mappings: 0, .. }}");
    assert!(rendered.contains("[redacted]"));
    assert!(!rendered.contains("s3cret"));
    assert_eq!(format!("{:?}", pending()), "PendingLogin([redacted])");
    assert_eq!(
        format!("{:?}", OidcLoginService::new(provider, TestStore::ok(), Vec::new())),
        expected
    );
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
        Err(OidcLoginError::Provider(OidcProviderError::TokenExchange(
            OidcTokenExchangeError::InvalidResponse { status: 200 }
        )))
    ));
    assert_eq!(store.calls(), 0);
}
