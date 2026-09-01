use std::net::{IpAddr, SocketAddr};

use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::http::HeaderName;
use rstest::rstest;

use super::{attach, resolved};
use crate::rate_limit::{RateLimitConfig, RateLimiter};

#[rstest]
#[case::no_peer_recorded(&[], None, &[], None)]
#[case::direct_peer(&[], Some("198.51.100.7"), &[], Some("198.51.100.7"))]
#[case::forged_forwarded_for_from_an_untrusted_peer(
    &["10.0.0.0/8"],
    Some("198.51.100.7"),
    &[("x-forwarded-for", "203.0.113.9")],
    Some("198.51.100.7"),
)]
#[case::forged_real_ip_from_an_untrusted_peer(
    &["10.0.0.0/8"],
    Some("198.51.100.7"),
    &[("x-real-ip", "203.0.113.9")],
    Some("198.51.100.7"),
)]
#[case::forwarded_for_through_a_trusted_proxy(
    &["10.0.0.0/8"],
    Some("10.0.0.1"),
    &[("x-forwarded-for", "203.0.113.9")],
    Some("203.0.113.9"),
)]
#[case::real_ip_through_a_trusted_proxy(
    &["10.0.0.0/8"],
    Some("10.0.0.1"),
    &[("x-real-ip", "203.0.113.9")],
    Some("203.0.113.9"),
)]
#[case::trusted_proxy_forwarding_nothing(&["10.0.0.0/8"], Some("10.0.0.1"), &[], Some("10.0.0.1"))]
#[case::fully_trusted_chain(
    &["10.0.0.0/8"],
    Some("10.0.0.1"),
    &[("x-forwarded-for", "10.0.0.2, 10.0.0.3")],
    Some("10.0.0.1"),
)]
#[case::malformed_chain_has_no_address(
    &["10.0.0.0/8"],
    Some("10.0.0.1"),
    &[("x-forwarded-for", "203.0.113.9, not-an-address")],
    None,
)]
fn test_attach_records_only_an_address_the_server_resolved(
    #[case] trusted_proxies: &[&str],
    #[case] peer: Option<&str>,
    #[case] forwarded: &[(&str, &str)],
    #[case] expected: Option<&str>,
) {
    let attached = attach(&limiter(trusted_proxies), request(peer, forwarded));

    assert_eq!(
        resolved(attached.extensions()),
        expected.map(|address| address.parse::<IpAddr>().unwrap())
    );
}

fn limiter(trusted_proxies: &[&str]) -> RateLimiter {
    RateLimiter::new(RateLimitConfig {
        trusted_proxies: trusted_proxies.iter().map(|network| network.parse().unwrap()).collect(),
        ..RateLimitConfig::enabled_defaults()
    })
}

fn request(peer: Option<&str>, forwarded: &[(&str, &str)]) -> Request {
    let mut request = Request::new(Body::empty());
    if let Some(peer) = peer {
        let peer: IpAddr = peer.parse().unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::new(peer, 41234)));
    }
    for (name, value) in forwarded {
        request
            .headers_mut()
            .append(HeaderName::from_bytes(name.as_bytes()).unwrap(), value.parse().unwrap());
    }
    request
}
