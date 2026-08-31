use rstest::rstest;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

const TIMEOUT: Duration = Duration::from_secs(5);
const ISSUER: &str = "https://idp.example/realms/main";

fn guarded(trusted_endpoint_hosts: &[&str]) -> GuardedOidcTransport {
    GuardedOidcTransport::new([ISSUER], trusted_endpoint_hosts, TIMEOUT).unwrap()
}

async fn keys_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/keys"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    server
}

fn keys_url(server: &MockServer, host: &str) -> Url {
    Url::parse(&format!("http://{host}:{}/keys", server.address().port())).unwrap()
}

#[rstest]
#[case::issuer_own_host("https://idp.example/token")]
#[case::public_host("https://accounts.other.example/token")]
#[case::public_literal("https://8.8.8.8/token")]
#[case::approved_private_host("https://keys.corp.internal/jwks")]
#[case::approved_private_literal("https://10.0.0.5/jwks")]
fn test_permitted_destinations(#[case] url: &str) {
    assert!(guarded(&["keys.corp.internal", "10.0.0.5"]).permits(&Url::parse(url).unwrap()));
}

#[rstest]
#[case::loopback("https://127.0.0.1/token")]
#[case::private("https://10.0.0.1/token")]
#[case::cloud_metadata("http://169.254.169.254/latest/meta-data/")]
#[case::unique_local("https://[fd00::1]/jwks")]
#[case::ipv6_loopback("https://[::1]/jwks")]
#[case::unapproved_scheme("file:///etc/passwd")]
fn test_refused_destinations(#[case] url: &str) {
    assert!(!guarded(&["keys.corp.internal", "10.0.0.5"]).permits(&Url::parse(url).unwrap()));
}

/// An operator whose issuer is itself private keeps reaching it without listing anything.
#[test]
fn test_private_issuer_host_is_trusted_without_listing() {
    let transport = GuardedOidcTransport::new(["https://10.0.0.9"], std::iter::empty::<&str>(), TIMEOUT).unwrap();

    assert!(transport.permits(&Url::parse("https://10.0.0.9/jwks").unwrap()));
}

#[rstest]
#[case::not_a_url("idp.example")]
#[case::no_host("file:///realms/main")]
fn test_new_rejects_an_issuer_without_a_host(#[case] issuer: &str) {
    assert_eq!(
        GuardedOidcTransport::new([issuer], std::iter::empty::<&str>(), TIMEOUT).unwrap_err(),
        GuardedOidcTransportError::Issuer
    );
}

#[rstest]
#[case::issuer(
    GuardedOidcTransportError::Issuer,
    "the configured OIDC issuer is not a URL carrying a host"
)]
#[case::client(GuardedOidcTransportError::Client, "the OIDC HTTP client could not be built")]
fn test_error_messages(#[case] error: GuardedOidcTransportError, #[case] expected: &str) {
    assert_eq!(error.to_string(), expected);
}

#[tokio::test]
async fn test_approved_literal_destination_is_fetched() {
    let server = keys_server().await;
    let transport = guarded(&["127.0.0.1"]);
    let url = keys_url(&server, "127.0.0.1");
    let request = transport.client().get(url).build().unwrap();

    assert_eq!(transport.execute(request).await.unwrap().status(), 200);
}

/// A hostname clears the pre-connection check, so the resolver is what has to refuse a name that
/// resolves into blocked address space.
#[tokio::test]
async fn test_hostname_resolving_into_blocked_space_never_connects() {
    let server = keys_server().await;
    let transport = guarded(&[]);
    let url = keys_url(&server, "localhost");
    assert!(transport.permits(&url));
    let request = transport.client().get(url).build().unwrap();

    let refused = transport.execute(request).await.is_err();

    assert_eq!((refused, server.received_requests().await.unwrap().len()), (true, 0));
}

#[tokio::test]
async fn test_approved_hostname_still_resolves_into_private_space() {
    let server = keys_server().await;
    let transport = guarded(&["localhost"]);
    let url = keys_url(&server, "localhost");
    let request = transport.client().get(url).build().unwrap();

    assert_eq!(transport.execute(request).await.unwrap().status(), 200);
}
