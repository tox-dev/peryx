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

#[rstest]
#[case::public_dns("8.8.8.8")]
#[case::outside_protocol_assignment("192.1.0.1")]
#[case::before_shared_range("100.63.255.255")]
#[case::after_shared_range("100.128.0.0")]
#[case::before_benchmark_range("198.17.255.255")]
#[case::after_benchmark_range("198.20.0.0")]
#[case::outside_documentation_range("[2001:db9::1]")]
#[case::v6_mapped_public("[::ffff:8.8.8.8]")]
#[case::v6_translation("[64:ff9b::1]")]
#[case::v6_pcp_anycast("[2001:1::1]")]
#[case::v6_turn_anycast("[2001:1::2]")]
#[case::v6_dns_sd_anycast("[2001:1::3]")]
#[case::v6_amt("[2001:3::1]")]
#[case::v6_as112("[2001:4:112::1]")]
#[case::v6_orchid_v2("[2001:20::1]")]
#[case::v6_det("[2001:30::1]")]
#[case::v6_direct_as112("[2620:4f:8000::1]")]
fn test_check_url_allows_global_literal(#[case] address: &str) {
    guard("https://pub.example.com/", &[])
        .check_url(&Url::parse(&format!("https://{address}/x")).unwrap())
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
#[case("http://[::]/x")]
#[case("http://[::1]/x")]
#[case("http://[::ffff:127.0.0.1]/x")]
#[case("http://[64:ff9b:1::1]/x")]
#[case("http://[100::1]/x")]
#[case("http://[100:0:0:1::1]/x")]
#[case("http://[2001::1]/x")]
#[case("http://[2001:1::4]/x")]
#[case("http://[2001:2::1]/x")]
#[case("http://[2001:4:113::1]/x")]
#[case("http://[2001:10::1]/x")]
#[case("http://[2001:40::1]/x")]
#[case("http://[2001:db8::1]/x")]
#[case("http://[2002::1]/x")]
#[case("http://[3fff::1]/x")]
#[case("http://[5f00::1]/x")]
#[case("http://[fc00::1]/x")]
#[case("http://[fe80::1]/x")]
#[case("http://[fec0::1]/x")]
#[case("http://[ff02::1]/x")]
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
    let base = Url::parse(base).unwrap();
    OutboundGuard::with_resolver(
        base.host_str()
            .map(str::to_owned)
            .into_iter()
            .chain(trusted.iter().map(|entry| (*entry).to_owned())),
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

#[rstest]
#[case::mapped_public("::ffff:8.8.8.8")]
#[case::translation("64:ff9b::1")]
#[case::pcp_anycast("2001:1::1")]
#[case::turn_anycast("2001:1::2")]
#[case::dns_sd_anycast("2001:1::3")]
#[case::amt("2001:3::1")]
#[case::as112("2001:4:112::1")]
#[case::orchid_v2("2001:20::1")]
#[case::det("2001:30::1")]
#[tokio::test]
async fn test_resolver_keeps_global_ipv6_exception(#[case] address: &str) {
    let socket = format!("[{address}]:80").parse().unwrap();
    let kept = resolve(
        &guard_with("https://pub.example.com/", &[], Ok(vec![socket])),
        "cdn.example.com",
    )
    .await
    .unwrap();

    assert_eq!(kept, vec![socket]);
}

#[rstest]
#[case::mapped_loopback("::ffff:127.0.0.1")]
#[case::translation_private("64:ff9b:1::1")]
#[case::discard_only("100::1")]
#[case::dummy("100:0:0:1::1")]
#[case::teredo("2001::1")]
#[case::benchmarking("2001:2::1")]
#[case::orchid_deprecated("2001:10::1")]
#[case::six_to_four("2002::1")]
#[case::documentation("3fff::1")]
#[case::segment_routing("5f00::1")]
#[case::site_local("fec0::1")]
#[tokio::test]
async fn test_resolver_rejects_non_global_ipv6(#[case] address: &str) {
    let socket = format!("[{address}]:80").parse().unwrap();
    let error = resolve(
        &guard_with("https://pub.example.com/", &[], Ok(vec![socket])),
        "cdn.example.com",
    )
    .await;

    assert!(error.is_err());
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

#[rstest]
#[case::conditional(ConditionalRequest::Etag, "http://127.0.0.1:1/x")]
#[case::validated(ConditionalRequest::LastModified, "http://169.254.169.254/latest/meta-data/")]
#[tokio::test]
async fn test_conditional_requests_block_private_literals(#[case] request: ConditionalRequest, #[case] url: &str) {
    let error = request
        .send(&blocked_client(), Url::parse(url).unwrap())
        .await
        .unwrap_err();

    assert!(matches!(error, UpstreamError::BlockedDestination { .. }));
}

#[derive(Clone, Copy)]
enum ConditionalRequest {
    Etag,
    LastModified,
}

impl ConditionalRequest {
    async fn send(self, client: &UpstreamClient, url: Url) -> Result<reqwest::Response, UpstreamError> {
        match self {
            Self::Etag => client.send_conditional(url, "application/json", Some("etag")).await,
            Self::LastModified => {
                client
                    .send_validated(url, "application/json", None, Some("Mon, 01 Jan 2024 00:00:00 GMT"))
                    .await
            }
        }
    }
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
async fn test_range_session_blocks_private_literal() {
    let error = blocked_client().range_session("http://127.0.0.1/x").await.unwrap_err();

    assert!(matches!(
        error,
        RangeError::Upstream(UpstreamError::BlockedDestination { .. })
    ));
}

/// Pinning does not carry a session past the outbound policy: every read re-checks its URL.
#[tokio::test]
async fn test_pinned_range_read_blocks_private_literal() {
    let session = crate::client::RangeSession::pinned(
        blocked_client(),
        Url::parse("http://127.0.0.1/x").unwrap(),
        11,
        "\"generation-a\"",
    );

    let error = session.fetch_range(0, 10, 11).await.unwrap_err();

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
