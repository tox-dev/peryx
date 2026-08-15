use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use rstest::rstest;
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::client::{RangeError, UpstreamClient, UpstreamError};

use super::{OutboundGuard, is_global_ip};

#[rstest]
#[case::v4_public("8.8.8.8", true)]
#[case::v4_public_low("1.1.1.1", true)]
#[case::v4_this_network("0.1.2.3", false)]
#[case::v4_private_ten("10.0.0.1", false)]
#[case::v4_private_172("172.16.0.1", false)]
#[case::v4_private_192("192.168.1.1", false)]
#[case::v4_shared_cgnat("100.64.0.1", false)]
#[case::v4_loopback("127.0.0.1", false)]
#[case::v4_metadata("169.254.169.254", false)]
#[case::v4_multicast("224.0.0.1", false)]
#[case::v4_broadcast("255.255.255.255", false)]
#[case::v4_documentation("192.0.2.1", false)]
#[case::v4_ietf("192.0.0.1", false)]
#[case::v4_benchmarking("198.18.0.1", false)]
#[case::v4_reserved("240.0.0.1", false)]
#[case::v6_public("2606:4700:4700::1111", true)]
#[case::v6_mapped_public("::ffff:8.8.8.8", true)]
#[case::v6_mapped_loopback("::ffff:127.0.0.1", false)]
#[case::v6_mapped_private("::ffff:10.0.0.1", false)]
#[case::v6_unspecified("::", false)]
#[case::v6_loopback("::1", false)]
#[case::v6_multicast("ff02::1", false)]
#[case::v6_link_local("fe80::1", false)]
#[case::v6_unique_local("fc00::1", false)]
#[case::v6_documentation("2001:db8::1", false)]
fn test_is_global_ip_classifies_addresses(#[case] ip: &str, #[case] expected: bool) {
    assert_eq!(is_global_ip(ip.parse().unwrap()), expected);
}

fn guard(base: &str, trusted: &[&str]) -> OutboundGuard {
    OutboundGuard::new(&Url::parse(base).unwrap(), trusted.iter().copied())
}

#[test]
fn test_debug_reports_trusted_hosts_without_the_resolver() {
    assert_eq!(
        format!("{:?}", guard("https://pub.example.com/", &[])),
        "OutboundGuard { trusted: {\"pub.example.com\"}, .. }"
    );
}

#[test]
fn test_check_url_rejects_non_http_scheme() {
    let error = guard("https://pub.example.com/", &[])
        .check_url(&Url::parse("ftp://pub.example.com/x").unwrap())
        .unwrap_err();

    assert!(matches!(error, UpstreamError::BlockedDestination { .. }));
}

#[test]
fn test_check_url_allows_global_literal() {
    guard("https://pub.example.com/", &[])
        .check_url(&Url::parse("https://8.8.8.8/x").unwrap())
        .unwrap();
}

#[test]
fn test_check_url_allows_domain_deferring_to_resolver() {
    guard("https://pub.example.com/", &[])
        .check_url(&Url::parse("https://assets.example.com/resource.bin").unwrap())
        .unwrap();
}

#[rstest]
#[case("http://127.0.0.1/x")]
#[case("http://10.0.0.1/x")]
#[case("http://169.254.169.254/latest/meta-data/")]
#[case("http://[::1]/x")]
fn test_check_url_blocks_non_global_literal(#[case] url: &str) {
    let error = guard("https://pub.example.com/", &[])
        .check_url(&Url::parse(url).unwrap())
        .unwrap_err();

    assert!(matches!(error, UpstreamError::BlockedDestination { .. }));
}

#[test]
fn test_check_url_trusts_configured_base_host() {
    guard("http://10.0.0.1/api/", &[])
        .check_url(&Url::parse("http://10.0.0.1:8080/files/artifact.bin").unwrap())
        .unwrap();
}

#[rstest]
#[case("http://10.0.0.5/artifact.bin")]
#[case("http://[fd00::1]/artifact.bin")]
fn test_check_url_trusts_allowlisted_private_host(#[case] url: &str) {
    guard("https://pub.example.com/", &["", "10.0.0.5", "[fd00::1]", "FILES.CORP"])
        .check_url(&Url::parse(url).unwrap())
        .unwrap();
}

#[test]
fn test_check_url_blocks_private_host_outside_allowlist() {
    let error = guard("https://pub.example.com/", &["10.0.0.5"])
        .check_url(&Url::parse("http://10.0.0.6/x").unwrap())
        .unwrap_err();

    assert!(matches!(error, UpstreamError::BlockedDestination { .. }));
}

struct FakeResolver(Result<Vec<SocketAddr>, ()>);

impl Resolve for FakeResolver {
    fn resolve(&self, _name: Name) -> Resolving {
        let outcome = self.0.clone();
        Box::pin(async move {
            match outcome {
                Ok(addrs) => Ok(Box::new(addrs.into_iter()) as Addrs),
                Err(()) => Err("fake resolver failure".into()),
            }
        })
    }
}

fn guard_with(base: &str, trusted: &[&str], outcome: Result<Vec<SocketAddr>, ()>) -> OutboundGuard {
    OutboundGuard::with_resolver(
        &Url::parse(base).unwrap(),
        trusted.iter().copied(),
        Arc::new(FakeResolver(outcome)),
    )
}

async fn resolve(guard: &OutboundGuard, host: &str) -> Result<Vec<SocketAddr>, ()> {
    let name: Name = host.parse().unwrap();
    guard.resolve(name).await.map(Iterator::collect).map_err(|_| ())
}

#[tokio::test]
async fn test_resolver_keeps_only_global_addresses_for_untrusted_host() {
    let addrs = vec!["10.0.0.1:80".parse().unwrap(), "8.8.8.8:80".parse().unwrap()];
    let kept = resolve(
        &guard_with("https://pub.example.com/", &[], Ok(addrs)),
        "cdn.example.com",
    )
    .await
    .unwrap();

    assert_eq!(kept, vec!["8.8.8.8:80".parse().unwrap()]);
}

#[tokio::test]
async fn test_resolver_rejects_host_that_only_resolves_privately() {
    let addrs = vec!["10.0.0.1:80".parse().unwrap()];
    let error = resolve(
        &guard_with("https://pub.example.com/", &[], Ok(addrs)),
        "cdn.example.com",
    )
    .await;

    assert!(error.is_err());
}

#[tokio::test]
async fn test_resolver_allows_private_addresses_for_trusted_host() {
    let addrs = vec!["10.0.0.1:80".parse().unwrap()];
    let kept = resolve(&guard_with("https://files.corp/", &[], Ok(addrs.clone())), "files.corp")
        .await
        .unwrap();

    assert_eq!(kept, addrs);
}

#[tokio::test]
async fn test_resolver_propagates_lookup_failure() {
    let error = resolve(&guard_with("https://pub.example.com/", &[], Err(())), "cdn.example.com").await;

    assert!(error.is_err());
}

#[tokio::test]
async fn test_system_resolver_filters_untrusted_loopback() {
    let error = resolve(&guard("https://pub.example.com/", &[]), "localhost").await;

    assert!(error.is_err());
}

#[tokio::test]
async fn test_system_resolver_returns_trusted_loopback() {
    let kept = resolve(&guard("http://localhost/api/", &[]), "localhost")
        .await
        .unwrap();

    assert!(!kept.is_empty());
}

#[tokio::test]
async fn test_system_resolver_reports_unresolvable_host() {
    let error = resolve(&guard("https://pub.example.com/", &[]), "peryx.nonexistent.invalid").await;

    assert!(error.is_err());
}

fn blocked_client() -> UpstreamClient {
    UpstreamClient::new("https://pub.example.com/api/").unwrap()
}

#[tokio::test]
async fn test_fetch_bytes_blocks_private_literal() {
    let error = blocked_client().fetch_bytes("http://127.0.0.1:1/x").await.unwrap_err();

    assert!(matches!(error, UpstreamError::BlockedDestination { .. }));
}

#[tokio::test]
async fn test_stream_bytes_blocks_private_literal() {
    let error = blocked_client().stream_bytes("http://10.0.0.1/x").await.err().unwrap();

    assert!(matches!(error, UpstreamError::BlockedDestination { .. }));
}

#[tokio::test]
async fn test_fetch_bytes_limited_blocks_metadata_address() {
    let error = blocked_client()
        .fetch_bytes_limited("http://169.254.169.254/x", 16)
        .await
        .unwrap_err();

    assert!(matches!(error, UpstreamError::BlockedDestination { .. }));
}

#[tokio::test]
async fn test_head_file_for_range_blocks_private_literal() {
    let error = blocked_client()
        .head_file_for_range("http://127.0.0.1/x")
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        RangeError::Upstream(UpstreamError::BlockedDestination { .. })
    ));
}

#[tokio::test]
async fn test_fetch_range_blocks_private_literal() {
    let error = blocked_client()
        .fetch_range("http://127.0.0.1/x", 0, 10)
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        RangeError::Upstream(UpstreamError::BlockedDestination { .. })
    ));
}

#[tokio::test]
async fn test_redirect_to_blocked_host_fails() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/a"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "http://10.0.0.1:9/evil"))
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/api/", server.uri())).unwrap();

    let error = client
        .fetch_bytes(&format!("{}/files/a", server.uri()))
        .await
        .unwrap_err();

    assert!(matches!(error, UpstreamError::Http(err) if err.is_redirect()));
}

#[tokio::test]
async fn test_redirect_to_same_host_is_followed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/a"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", format!("{}/files/b", server.uri())))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/b"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"payload".to_vec()))
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/api/", server.uri())).unwrap();

    let bytes = client.fetch_bytes(&format!("{}/files/a", server.uri())).await.unwrap();

    assert_eq!(&bytes[..], b"payload");
}

#[tokio::test]
async fn test_redirect_loop_stops_at_limit() {
    let server = MockServer::start().await;
    let target = format!("{}/loop", server.uri());
    Mock::given(method("GET"))
        .and(path("/loop"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", target))
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/api/", server.uri())).unwrap();

    let error = client.fetch_bytes(&format!("{}/loop", server.uri())).await.unwrap_err();

    assert_eq!(error.status(), None);
    assert_eq!(error.user_message(), "upstream request failed");
    let reasons = std::iter::successors(std::error::Error::source(&error), |cause| cause.source())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    assert!(
        reasons.iter().any(|reason| reason == "too many redirects"),
        "{reasons:?}"
    );
}
