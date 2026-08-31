use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, Response, StatusCode, header};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use peryx_driver::state::AppState;
use peryx_identity::{
    ExternalGroup, ExternalGroupGrant, ExternalIdentity, ExternalLinkRequest, ExternalSubject, GrantScope,
    ManagedRoleGrant, OidcHttpTransport, OidcLoginProvider, OidcLoginService, OidcProviderSettings, PRE_AUTH_COOKIE,
    PendingLogin, ProviderId, Role, RoleGrant, SESSION_COOKIE, ServerUser, SessionSealer, UserId, UserName, UserState,
};
use rstest::rstest;
use serde_json::{Value, json};
use tower::ServiceExt as _;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &[u8] = b"a-token-realm-signing-secret-here";
const VALID_UNTIL: i64 = 4_102_444_800;
const NOW: i64 = VALID_UNTIL - 600;
const MODULUS: &str = "yRE6rHuNR0QbHO3H3Kt2pOKGVhQqGZXInOduQNxXzuKlvQTLUTv4l4sggh5_CYYi_cvI-SXVT9kPWSKXxJXBXd_4LkvcPuUakBoAkfh-eiFVMh2VrUyWyj3MFl0HTVF9KwRXLAcwkREiS3npThHRyIxuy0ZMeZfxVL5arMhw1SRELB8HoGfG_AtH89BIE9jDBHZ9dLelK9a184zAf8LwoPLxvJb3Il5nncqPcSfKDDodMFBIMc4lQzDKL5gvmiXLXB1AGLm8KBjfE8s3L5xqi-yUod-j8MtvIj812dkS4QMiRVN_by2h3ZY8LYVGrqZXZTcgn2ujn8uKjXLZVD5TdQ";
const PRIVATE_KEY_DER: &str = "MIIEpAIBAAKCAQEAyRE6rHuNR0QbHO3H3Kt2pOKGVhQqGZXInOduQNxXzuKlvQTLUTv4l4sggh5/CYYi/cvI+SXVT9kPWSKXxJXBXd/4LkvcPuUakBoAkfh+eiFVMh2VrUyWyj3MFl0HTVF9KwRXLAcwkREiS3npThHRyIxuy0ZMeZfxVL5arMhw1SRELB8HoGfG/AtH89BIE9jDBHZ9dLelK9a184zAf8LwoPLxvJb3Il5nncqPcSfKDDodMFBIMc4lQzDKL5gvmiXLXB1AGLm8KBjfE8s3L5xqi+yUod+j8MtvIj812dkS4QMiRVN/by2h3ZY8LYVGrqZXZTcgn2ujn8uKjXLZVD5TdQIDAQABAoIBAHREk0I0O9DvECKdWUpAmF3mY7oY9PNQiu44Yaf+AoSuyRpRUGTMIgc3u3eivOE8ALX0BmYUO5JtuRNZDpvt4SAwqCnVUinIf6C+eH/wSurCpapSM0BAHp4aOA7igptyOMgMPYBHNA1e9A7jE0dCxKWMl3DSWNyjQTk4zeRGEAEfbNjHrq6YCtjHSZSLmWiG80hnfnYos9hOr5JnLnyS7ZmFE/5P3XVrxLc/tQ5zum0R4cbrgzHiQP5RgfxGJaEi7XcgherCCOgurJSSbYH29Gz8u5fFbS+Yg8s+OiCss3cs1rSgJ9/eHZuzGEdUZVARH6hVMjSuwvqVTFaE8AgtleECgYEA+uLMn4kNqHlJS2A5uAnCkj90ZxEtNm3E8hAxUrhssktY5XSOAPBlxyf5RuRGIImGtUVIr4HuJSa5TX48n3Vdt9MYCprO/iYl6moNRSPt5qowIIOJmIjY2mqPDfDt/zw+fcDD3lmCJrFlzcnh0uea1CohxEbQnL3cypeLt+WbU6kCgYEAzSp19m1ajieFkqgoB0YTpt/OroDx38vvI5unInJlEeOjQ+oIAQdN2wpxBvTrRorMU6P07mFUbt1j+Co6CbNiw+X8HcCaqYLR5clbJOOWNR36PuzOpQLkfK8woupBxzW9B8gZmY8rB1mbJ+/WTPrEJy6YGmIEBkWylQ2VpW8O4O0CgYEApdbvvfFBlwD9YxbrcGz7MeNCFbMz+MucqQntIKoKJ91ImPxvtc0y6e/Rhnv0oyNlaUOwJVu0yNgNG117w0g4t/+Q38mvVC5xV7/cn7x9UMFk6MkqVir3dYGEqIl/OP1grY2Tq9HtB5iyG9L8NIamQOLMyUqqMUILxdthHyFmiGkCgYEAn9+PjpjGMPHxL0gj8Q8VbzsFtou6b1deIRRA2CHmSltltR1gYVTMwXxQeUhPMmgkMqUXzs4/WijgpthY44hK1TaZEKIuoxrS70nJ4WQLf5a9k1065fDsFZD6yGjdGxvwEmlGMZgTwqV7t1I4X0Ilqhav5hcs5apYL7gnPYPeRz0CgYALHCj/Ji8XSsDoF/MhVhnGdIs2P99NNdmo3R2Pv0CuZbDKMU559LJHUvrKS8WkuWRDuKrz1W/EQKApFjDGpdqToZqriUFQzwy7mR3ayIiogzNtHcvbDHx8oFnGY0OFksX/ye0/XGpy2SFxYRwGU98HPYeBvAQQrVjdkzfy7BmXQQ==";

fn user() -> ServerUser {
    ServerUser {
        id: UserId::random(),
        name: UserName::new("Ada Lovelace").unwrap(),
        state: UserState::Active,
        revision: 1,
    }
}

fn empty_state() -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::with_clock(
        peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        peryx_storage::blob::BlobStore::new(dir.path().join("blobs")),
        60,
        Vec::new(),
        Arc::new(|| NOW),
    );
    (dir, state)
}

fn settings(destination: &str) -> OidcProviderSettings {
    OidcProviderSettings {
        id: ProviderId::new("corporate").unwrap(),
        issuer: secure_origin(destination),
        client_id: "peryx-web".to_owned(),
        client_secret: None,
        redirect_uri: url::Url::parse("https://repository.example/_/login/corporate/callback").unwrap(),
        scopes: vec!["email".to_owned()],
        subject_claim: "sub".to_owned(),
        display_name_claim: "name".to_owned(),
        groups_claim: None,
        clock_skew: Duration::from_mins(1),
    }
}

fn provider(destination: &str) -> OidcLoginProvider {
    OidcLoginProvider::new(settings(destination), transport(destination)).unwrap()
}

fn provider_with_id(id: &str, destination: &str) -> OidcLoginProvider {
    let mut settings = settings(destination);
    settings.id = ProviderId::new(id).unwrap();
    settings.redirect_uri.set_path(&format!("/_/login/{id}/callback"));
    OidcLoginProvider::new(settings, transport(destination)).unwrap()
}

fn transport(destination: &str) -> Arc<dyn OidcHttpTransport> {
    Arc::new(WiremockTransport {
        logical_origin: url::Url::parse(&secure_origin(destination)).unwrap(),
        destination: url::Url::parse(destination).unwrap(),
        client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap(),
    })
}

fn secure_origin(origin: &str) -> String {
    let mut url = url::Url::parse(origin).unwrap();
    url.set_scheme("https").unwrap();
    url.to_string().trim_end_matches('/').to_owned()
}

#[derive(Debug)]
struct WiremockTransport {
    logical_origin: url::Url,
    destination: url::Url,
    client: reqwest::Client,
}

#[async_trait::async_trait]
impl OidcHttpTransport for WiremockTransport {
    fn client(&self) -> &reqwest::Client {
        &self.client
    }

    fn permits(&self, _url: &url::Url) -> bool {
        true
    }

    async fn execute(&self, mut request: reqwest::Request) -> Result<reqwest::Response, reqwest::Error> {
        if request.url().origin() == self.logical_origin.origin() {
            request.url_mut().set_scheme(self.destination.scheme()).unwrap();
            request.url_mut().set_host(self.destination.host_str()).unwrap();
            request.url_mut().set_port(self.destination.port()).unwrap();
        }
        self.client.execute(request).await
    }
}

fn state_with_provider(destination: &str) -> (tempfile::TempDir, Arc<AppState>) {
    state_with_providers(vec![provider(destination)])
}

fn state_with_providers(providers: Vec<OidcLoginProvider>) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let mut state = AppState::with_clock(
        meta.clone(),
        peryx_storage::blob::BlobStore::new(dir.path().join("blobs")),
        60,
        Vec::new(),
        Arc::new(|| NOW),
    );
    assert!(state.set_session_sealer(SessionSealer::new(KEY)).is_ok());
    assert!(
        state
            .set_oidc_logins(providers.into_iter().map(|provider| OidcLoginService::new(
                provider,
                meta.clone(),
                Vec::new()
            )))
            .is_ok()
    );
    (dir, Arc::new(state))
}

async fn send(state: Arc<AppState>, method: Method, uri: &str, cookie: Option<&str>) -> Response<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    crate::router(state)
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

fn set_cookies(response: &Response<Body>) -> Vec<String> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap().to_owned())
        .collect()
}

fn session_cookie(user: &ServerUser) -> String {
    format!(
        "{SESSION_COOKIE}={}",
        SessionSealer::new(KEY).seal_session(user, VALID_UNTIL)
    )
}

fn pre_auth_cookie(state: &str) -> String {
    let pending = PendingLogin {
        provider: ProviderId::new("corporate").unwrap(),
        state: state.to_owned(),
        nonce: "n".to_owned(),
        verifier: "v".to_owned(),
        challenge: "c".to_owned(),
    };
    format!(
        "{PRE_AUTH_COOKIE}={}",
        SessionSealer::new(KEY).seal_pre_auth(&pending, VALID_UNTIL)
    )
}

fn location(response: &Response<Body>) -> String {
    response.headers()[header::LOCATION].to_str().unwrap().to_owned()
}

async fn body_json(response: Response<Body>) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn body_text(response: Response<Body>) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn test_login_start_for_an_unknown_provider_is_not_found() {
    let (_dir, state) = empty_state();
    let response = send(Arc::new(state), Method::GET, "/_/login/ghost", None).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_login_start_without_a_configured_sealer_is_a_server_error() {
    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let mut state = AppState::with_clock(
        meta.clone(),
        peryx_storage::blob::BlobStore::new(dir.path().join("blobs")),
        60,
        Vec::new(),
        Arc::new(|| NOW),
    );
    let provider = provider("http://issuer.invalid/");
    assert!(
        state
            .set_oidc_logins([OidcLoginService::new(provider, meta, Vec::new())])
            .is_ok()
    );
    let response = send(Arc::new(state), Method::GET, "/_/login/corporate", None).await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn test_login_start_maps_an_unreachable_provider_to_service_unavailable() {
    let (_dir, state) = state_with_provider("http://127.0.0.1:1/");
    let response = send(state, Method::GET, "/_/login/corporate", None).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_callback_without_a_configured_sealer_is_a_server_error() {
    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let mut state = AppState::new(
        meta.clone(),
        peryx_storage::blob::BlobStore::new(dir.path().join("blobs")),
        60,
        Vec::new(),
    );
    let provider = provider("http://issuer.invalid/");
    assert!(
        state
            .set_oidc_logins([OidcLoginService::new(provider, meta, Vec::new())])
            .is_ok()
    );
    let response = send(
        Arc::new(state),
        Method::GET,
        "/_/login/corporate/callback?state=s&code=c",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn test_a_disabled_linked_user_surfaces_a_store_error() {
    let server = MockServer::start().await;
    let issuer = secure_origin(&server.uri());
    mount_issuer(&server, &issuer).await;
    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let linked = meta
        .link_external_identity(ExternalLinkRequest {
            identity: ExternalIdentity::new(
                ProviderId::new("corporate").unwrap(),
                ExternalSubject::new("subject-123").unwrap(),
            ),
            display_name: UserName::new("Grace Hopper").unwrap(),
            grants: Vec::new(),
        })
        .unwrap();
    meta.set_user_state(&linked.user.id, UserState::Disabled).unwrap();

    let mut state = AppState::with_clock(
        meta.clone(),
        peryx_storage::blob::BlobStore::new(dir.path().join("blobs")),
        60,
        Vec::new(),
        Arc::new(|| NOW),
    );
    let provider = provider(&server.uri());
    assert!(state.set_session_sealer(SessionSealer::new(KEY)).is_ok());
    let authorization = provider.authorization(NOW).await.unwrap();
    let id_token = mint(&issuer, &authorization.pending.nonce);
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "at", "token_type": "Bearer", "id_token": id_token,
        })))
        .mount(&server)
        .await;
    assert!(
        state
            .set_oidc_logins([OidcLoginService::new(provider, meta, Vec::new())])
            .is_ok()
    );
    let state = Arc::new(state);

    let cookie = format!(
        "{PRE_AUTH_COOKIE}={}",
        SessionSealer::new(KEY).seal_pre_auth(&authorization.pending, VALID_UNTIL)
    );
    let uri = format!(
        "/_/login/corporate/callback?state={}&code=auth-code",
        authorization.pending.state
    );
    let response = send(state, Method::GET, &uri, Some(&cookie)).await;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn test_callback_for_an_unknown_provider_is_not_found() {
    let (_dir, state) = empty_state();
    let response = send(
        Arc::new(state),
        Method::GET,
        "/_/login/ghost/callback?state=s&code=c",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_callback_without_a_pre_auth_cookie_is_a_bad_request() {
    let (_dir, state) = state_with_provider("http://issuer.invalid/");
    let response = send(state, Method::GET, "/_/login/corporate/callback?state=s&code=c", None).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_callback_rejects_a_handoff_from_another_provider_without_contacting_it() {
    let corporate = MockServer::start().await;
    let partner = MockServer::start().await;
    let corporate_uri = corporate.uri();
    mount_issuer(&corporate, &secure_origin(&corporate_uri)).await;
    let (_dir, state) = state_with_providers(vec![
        provider(&corporate_uri),
        provider_with_id("partner", &partner.uri()),
    ]);
    let start = send(state.clone(), Method::GET, "/_/login/corporate", None).await;
    let redirect = location(&start);
    let pending_state = url::Url::parse(&redirect)
        .unwrap()
        .query_pairs()
        .find_map(|(key, value)| (key == "state").then(|| value.into_owned()))
        .unwrap();
    let cookie = set_cookies(&start)
        .into_iter()
        .find_map(|cookie| {
            cookie
                .starts_with(&format!("{PRE_AUTH_COOKIE}="))
                .then(|| cookie.split(';').next().unwrap().to_owned())
        })
        .unwrap();

    let response = send(
        state,
        Method::GET,
        &format!("/_/login/partner/callback?state={pending_state}&code=c"),
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        set_cookies(&response)
            .iter()
            .any(|cookie| cookie.starts_with(&format!("{PRE_AUTH_COOKIE}=;")) && cookie.contains("Max-Age=0"))
    );
    assert_eq!(
        body_text(response).await,
        "the login session is missing or has expired; start again"
    );
    assert!(partner.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_callback_rejects_and_clears_a_legacy_handoff() {
    let server = MockServer::start().await;
    let (_dir, state) = state_with_provider(&server.uri());
    let cookie = format!(
        "{PRE_AUTH_COOKIE}={}",
        SessionSealer::new(KEY).seal_pre_auth(
            &json!({ "state": "expected", "nonce": "n", "verifier": "v", "challenge": "c" }),
            VALID_UNTIL,
        )
    );

    let response = send(
        state,
        Method::GET,
        "/_/login/corporate/callback?state=expected&code=c",
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        set_cookies(&response)
            .iter()
            .any(|cookie| cookie.starts_with(&format!("{PRE_AUTH_COOKIE}=;")) && cookie.contains("Max-Age=0"))
    );
    assert_eq!(
        body_text(response).await,
        "the login session is missing or has expired; start again"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_callback_with_a_mismatched_state_fails_authentication() {
    let (_dir, state) = state_with_provider("http://issuer.invalid/");
    let response = send(
        state,
        Method::GET,
        "/_/login/corporate/callback?state=forged&code=c",
        Some(&pre_auth_cookie("expected")),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
}

#[tokio::test]
async fn test_callback_error_clears_the_matching_handoff_without_echoing_provider_detail() {
    let (_dir, state) = state_with_provider("http://127.0.0.1:1/");
    let response = send(
        state,
        Method::GET,
        concat!(
            "/_/login/corporate/callback?error=access_denied&state=expected",
            "&error_description=provider-secret&error_uri=https%3A%2F%2Fprovider.example%2Fsecret"
        ),
        Some(&pre_auth_cookie("expected")),
    )
    .await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert!(
        set_cookies(&response)
            .iter()
            .any(|cookie| cookie.starts_with(&format!("{PRE_AUTH_COOKIE}=;")) && cookie.contains("Max-Age=0"))
    );
    assert_eq!(body_text(response).await, "authentication was denied");
}

#[rstest]
#[case::interaction(
    "interaction_required",
    StatusCode::UNAUTHORIZED,
    "authentication requires user interaction"
)]
#[case::provider(
    "temporarily_unavailable",
    StatusCode::SERVICE_UNAVAILABLE,
    "the login provider is unavailable"
)]
#[case::unknown("provider_extension", StatusCode::UNAUTHORIZED, "authentication failed")]
#[tokio::test]
async fn test_callback_maps_authorization_errors(
    #[case] error: &str,
    #[case] status: StatusCode,
    #[case] message: &str,
) {
    let (_dir, state) = state_with_provider("http://127.0.0.1:1/");
    let response = send(
        state,
        Method::GET,
        &format!("/_/login/corporate/callback?error={error}&state=expected"),
        Some(&pre_auth_cookie("expected")),
    )
    .await;

    assert_eq!(response.status(), status);
    assert_eq!(body_text(response).await, message);
}

#[tokio::test]
async fn test_callback_error_with_a_mismatched_state_preserves_the_handoff() {
    let (_dir, state) = state_with_provider("http://127.0.0.1:1/");
    let response = send(
        state,
        Method::GET,
        "/_/login/corporate/callback?error=access_denied&state=forged",
        Some(&pre_auth_cookie("expected")),
    )
    .await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(set_cookies(&response).is_empty());
}

#[rstest]
#[case::both("state=s&code=c&error=access_denied")]
#[case::missing_state("error=access_denied")]
#[case::missing_result("state=s")]
#[case::repeated_state("state=s&state=s&code=c")]
#[case::repeated_code("state=s&code=c&code=c")]
#[case::repeated_error("state=s&error=access_denied&error=server_error")]
#[tokio::test]
async fn test_callback_rejects_an_invalid_response_shape(#[case] query: &str) {
    let (_dir, state) = state_with_provider("http://127.0.0.1:1/");
    let response = send(
        state,
        Method::GET,
        &format!("/_/login/corporate/callback?{query}"),
        None,
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_text(response).await, "invalid authentication response");
}

#[tokio::test]
async fn test_callback_maps_an_unreachable_provider_to_service_unavailable() {
    let (_dir, state) = state_with_provider("http://127.0.0.1:1/");
    let pending = PendingLogin {
        provider: ProviderId::new("corporate").unwrap(),
        state: "s".to_owned(),
        nonce: "n".to_owned(),
        verifier: "v".to_owned(),
        challenge: "c".to_owned(),
    };
    let cookie = format!(
        "{PRE_AUTH_COOKIE}={}",
        SessionSealer::new(KEY).seal_pre_auth(&pending, VALID_UNTIL)
    );
    let response = send(
        state,
        Method::GET,
        "/_/login/corporate/callback?state=s&code=c",
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[rstest]
#[case::invalid_grant(
    400,
    "application/json",
    concat!(
        r#"{"error":"invalid_grant","error_description":"provider-secret","#,
        r#""access_token":"token-secret","error_uri":"https://provider.example/secret"}"#
    ),
    StatusCode::UNAUTHORIZED,
    "authentication failed"
)]
#[case::server_invalid_grant(
    500,
    "application/json",
    r#"{"error":"invalid_grant"}"#,
    StatusCode::BAD_GATEWAY,
    "the login provider returned an invalid response"
)]
#[case::invalid_client(
    401,
    "application/json",
    r#"{"error":"invalid_client"}"#,
    StatusCode::BAD_GATEWAY,
    "the login provider returned an invalid response"
)]
#[case::success_invalid_grant(
    200,
    "application/json",
    r#"{"error":"invalid_grant"}"#,
    StatusCode::BAD_GATEWAY,
    "the login provider returned an invalid response"
)]
#[case::wrong_content_type(
    400,
    "text/plain",
    r#"{"error":"invalid_grant"}"#,
    StatusCode::BAD_GATEWAY,
    "the login provider returned an invalid response"
)]
#[case::malformed(
    400,
    "application/json",
    r#"{"error":"invalid_grant"#,
    StatusCode::BAD_GATEWAY,
    "the login provider returned an invalid response"
)]
#[case::html(
    500,
    "text/html",
    "<h1>provider-secret</h1>",
    StatusCode::BAD_GATEWAY,
    "the login provider returned an invalid response"
)]
#[case::missing_id_token(
    200,
    "application/json",
    r#"{"token_type":"Bearer"}"#,
    StatusCode::BAD_GATEWAY,
    "the login provider returned an invalid response"
)]
#[tokio::test]
async fn test_callback_classifies_token_endpoint_failures_without_exposing_provider_text(
    #[case] provider_status: u16,
    #[case] content_type: &str,
    #[case] provider_body: &str,
    #[case] expected_status: StatusCode,
    #[case] expected_body: &str,
) {
    let server = MockServer::start().await;
    let issuer = secure_origin(&server.uri());
    mount_issuer(&server, &issuer).await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(provider_status).set_body_raw(provider_body, content_type))
        .mount(&server)
        .await;
    let (_dir, state) = state_with_provider(&server.uri());

    let response = send(
        state,
        Method::GET,
        "/_/login/corporate/callback?state=expected&code=auth-code",
        Some(&pre_auth_cookie("expected")),
    )
    .await;

    assert_eq!(response.status(), expected_status);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(body_text(response).await, expected_body);
}

#[tokio::test]
async fn test_callback_is_rejected_on_a_read_only_replica() {
    let (_dir, mut state) = empty_state();
    state.set_read_only(true).unwrap();

    let response = send(
        Arc::new(state),
        Method::GET,
        "/_/login/corporate/callback?state=s&code=c",
        None,
    )
    .await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_session_without_a_cookie_reports_no_user() {
    let (dir, mut state) = empty_state();
    assert!(state.set_session_sealer(SessionSealer::new(KEY)).is_ok());
    let _keep = dir;
    let response = send(Arc::new(state), Method::GET, "/_/session", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["user"], Value::Null);
}

#[tokio::test]
async fn test_session_without_a_configured_sealer_reports_no_user() {
    let (_dir, state) = empty_state();
    let sealed = SessionSealer::new(KEY).seal_session(&user(), VALID_UNTIL);
    let cookie = format!("{SESSION_COOKIE}={sealed}");
    let response = send(Arc::new(state), Method::GET, "/_/session", Some(&cookie)).await;
    assert_eq!(body_json(response).await["user"], Value::Null);
}

#[tokio::test]
async fn test_session_with_a_valid_cookie_returns_the_user() {
    let (_dir, mut state) = empty_state();
    assert!(state.set_session_sealer(SessionSealer::new(KEY)).is_ok());
    let user = state.serving.users.create("Ada Lovelace").unwrap();
    let cookie = session_cookie(&user);

    let response = send(Arc::new(state), Method::GET, "/_/session", Some(&cookie)).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["user"]["name"], "Ada Lovelace");
    assert_eq!(body["user"]["id"], user.id.as_str());
}

/// The sealed cookie carries a snapshot of the account, so the stored record has to decide.
#[tokio::test]
async fn test_session_ignores_a_cookie_for_a_disabled_account() {
    let (_dir, mut state) = empty_state();
    assert!(state.set_session_sealer(SessionSealer::new(KEY)).is_ok());
    let user = state.serving.users.create("Ada Lovelace").unwrap();
    let cookie = session_cookie(&user);
    state.serving.users.disable(&user.id).unwrap();

    let response = send(Arc::new(state), Method::GET, "/_/session", Some(&cookie)).await;

    assert_eq!(body_json(response).await["user"], Value::Null);
}

#[tokio::test]
async fn test_session_ignores_a_cookie_for_an_account_this_server_never_stored() {
    let (_dir, mut state) = empty_state();
    assert!(state.set_session_sealer(SessionSealer::new(KEY)).is_ok());
    let cookie = session_cookie(&user());

    let response = send(Arc::new(state), Method::GET, "/_/session", Some(&cookie)).await;

    assert_eq!(body_json(response).await["user"], Value::Null);
}

/// A browser session authenticates reads only; a management mutation still needs an
/// `Authorization` credential, which is what keeps the cookie off the CSRF surface.
#[tokio::test]
async fn test_a_session_cookie_cannot_authorize_a_management_mutation() {
    let (_dir, mut state) = empty_state();
    assert!(state.set_session_sealer(SessionSealer::new(KEY)).is_ok());
    let administrator = state.serving.users.create("Ada Lovelace").unwrap();
    state
        .serving
        .authorization
        .grant(&administrator.id, Role::Administrator, GrantScope::Server)
        .unwrap();
    let cookie = session_cookie(&administrator);

    let response = crate::router(Arc::new(state))
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/+grants")
                .header(header::COOKIE, &cookie)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "user": administrator.id.as_str(),
                        "role": "repository_reader",
                        "scope": {"kind": "repository", "name": "packages"},
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_session_with_an_expired_cookie_reports_no_user() {
    let (dir, mut state) = empty_state();
    assert!(state.set_session_sealer(SessionSealer::new(KEY)).is_ok());
    let _keep = dir;
    let sealed = SessionSealer::new(KEY).seal_session(&user(), 1);
    let cookie = format!("{SESSION_COOKIE}={sealed}");
    let response = send(Arc::new(state), Method::GET, "/_/session", Some(&cookie)).await;
    assert_eq!(body_json(response).await["user"], Value::Null);
}

#[tokio::test]
async fn test_session_lists_the_configured_providers() {
    let (_dir, state) = state_with_provider("http://issuer.invalid/");
    let response = send(state, Method::GET, "/_/session", None).await;
    assert_eq!(body_json(response).await["providers"], json!(["corporate"]));
}

#[tokio::test]
async fn test_logout_clears_the_session_cookie() {
    let (_dir, state) = empty_state();
    let response = send(Arc::new(state), Method::POST, "/_/logout", None).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/");
    let cleared = set_cookies(&response);
    assert!(
        cleared
            .iter()
            .any(|c| c.starts_with(&format!("{SESSION_COOKIE}=;")) && c.contains("Max-Age=0")),
        "{cleared:?}"
    );
}

#[tokio::test]
async fn test_logout_is_allowed_on_a_read_only_replica() {
    let (_dir, mut state) = empty_state();
    state.set_read_only(true).unwrap();
    let response = send(Arc::new(state), Method::POST, "/_/logout", None).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
}

fn encoding_key() -> EncodingKey {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    EncodingKey::from_rsa_der(&STANDARD.decode(PRIVATE_KEY_DER).unwrap())
}

fn mint(issuer: &str, nonce: &str) -> String {
    mint_with_groups(issuer, nonce, None)
}

fn mint_with_groups(issuer: &str, nonce: &str, groups: Option<Value>) -> String {
    let mut claims = json!({
        "iss": issuer,
        "aud": "peryx-web",
        "exp": VALID_UNTIL,
        "iat": VALID_UNTIL - 3600,
        "nonce": nonce,
        "sub": "subject-123",
        "name": "Grace Hopper",
    });
    if let Some(groups) = groups {
        claims["groups"] = groups;
    }
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("k1".to_owned());
    jsonwebtoken::encode(&header, &claims, &encoding_key()).unwrap()
}

async fn mount_issuer(server: &MockServer, issuer: &str) {
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/authorize"),
            "token_endpoint": format!("{issuer}/token"),
            "jwks_uri": format!("{issuer}/jwks"),
            "id_token_signing_alg_values_supported": ["RS256"],
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/jwks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "keys": [{"kty": "RSA", "n": MODULUS, "e": "AQAB", "kid": "k1", "alg": "RS256", "use": "sig"}]
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn test_login_start_redirects_to_the_provider_and_seals_the_handoff() {
    let server = MockServer::start().await;
    let issuer = secure_origin(&server.uri());
    mount_issuer(&server, &issuer).await;
    let (_dir, state) = state_with_provider(&server.uri());

    let response = send(state, Method::GET, "/_/login/corporate", None).await;

    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert!(location(&response).starts_with(&format!("{issuer}/authorize")));
    let cookies = set_cookies(&response);
    assert!(
        cookies.iter().any(|c| c.starts_with(&format!("{PRE_AUTH_COOKIE}="))
            && c.contains("HttpOnly")
            && c.contains("SameSite=Lax")),
        "{cookies:?}"
    );
}

#[rstest]
#[case::single_provider(false)]
#[case::matching_provider_among_multiple(true)]
#[tokio::test]
async fn test_a_valid_callback_creates_a_session(#[case] multiple_providers: bool) {
    let server = MockServer::start().await;
    let issuer = secure_origin(&server.uri());
    mount_issuer(&server, &issuer).await;
    let provider = provider(&server.uri());

    let authorization = provider.authorization(NOW).await.unwrap();
    let id_token = mint(&issuer, &authorization.pending.nonce);
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "at", "token_type": "Bearer", "id_token": id_token,
        })))
        .mount(&server)
        .await;
    let mut providers = vec![provider];
    if multiple_providers {
        providers.push(provider_with_id("partner", &server.uri()));
    }
    let (_dir, state) = state_with_providers(providers);

    let cookie = format!(
        "{PRE_AUTH_COOKIE}={}",
        SessionSealer::new(KEY).seal_pre_auth(&authorization.pending, VALID_UNTIL)
    );
    let uri = format!(
        "/_/login/corporate/callback?state={}&code=auth-code",
        authorization.pending.state
    );
    let response = send(state, Method::GET, &uri, Some(&cookie)).await;

    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/");
    let cookies = set_cookies(&response);
    assert!(
        cookies
            .iter()
            .any(|c| c.starts_with(&format!("{SESSION_COOKIE}=")) && c.contains("HttpOnly")),
        "{cookies:?}"
    );
    assert!(
        cookies
            .iter()
            .any(|c| c.starts_with(&format!("{PRE_AUTH_COOKIE}=;")) && c.contains("Max-Age=0")),
        "pre-auth not cleared: {cookies:?}"
    );
}

#[tokio::test]
async fn test_a_malformed_group_claim_preserves_managed_grants() {
    let server = MockServer::start().await;
    let issuer = secure_origin(&server.uri());
    mount_issuer(&server, &issuer).await;
    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let linked = meta
        .link_external_identity(ExternalLinkRequest {
            identity: ExternalIdentity::new(
                ProviderId::new("corporate").unwrap(),
                ExternalSubject::new("subject-123").unwrap(),
            ),
            display_name: UserName::new("Grace Hopper").unwrap(),
            grants: vec![ManagedRoleGrant {
                role: Role::RepositoryReader,
                scope: GrantScope::Repository {
                    name: "packages".to_owned(),
                },
            }],
        })
        .unwrap();
    let mut state = AppState::with_clock(
        meta.clone(),
        peryx_storage::blob::BlobStore::new(dir.path().join("blobs")),
        60,
        Vec::new(),
        Arc::new(|| NOW),
    );
    let mut provider_settings = settings(&server.uri());
    provider_settings.groups_claim = Some("groups".to_owned());
    let provider = OidcLoginProvider::new(provider_settings, transport(&server.uri())).unwrap();
    assert!(state.set_session_sealer(SessionSealer::new(KEY)).is_ok());
    let authorization = provider.authorization(NOW).await.unwrap();
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "at",
            "token_type": "Bearer",
            "id_token": mint_with_groups(
                &issuer,
                &authorization.pending.nonce,
                Some(json!(["release-admin", {"name": "developers"}])),
            ),
        })))
        .mount(&server)
        .await;
    assert!(
        state
            .set_oidc_logins([OidcLoginService::new(
                provider,
                meta.clone(),
                vec![ExternalGroupGrant {
                    group: ExternalGroup::new("release-admin").unwrap(),
                    role: Role::Operator,
                    scope: GrantScope::Server,
                }],
            )])
            .is_ok()
    );
    let response = send(
        Arc::new(state),
        Method::GET,
        &format!(
            "/_/login/corporate/callback?state={}&code=auth-code",
            authorization.pending.state
        ),
        Some(&format!(
            "{PRE_AUTH_COOKIE}={}",
            SessionSealer::new(KEY).seal_pre_auth(&authorization.pending, VALID_UNTIL)
        )),
    )
    .await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        meta.user_role_grants(&linked.user.id).unwrap(),
        vec![RoleGrant::new(
            linked.user.id,
            Role::RepositoryReader,
            GrantScope::Repository {
                name: "packages".to_owned(),
            },
        )]
    );
}
